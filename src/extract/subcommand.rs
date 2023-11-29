use anyhow::{anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread;

use crate::command_utils::{
    get_serial_reader, get_threshold_from_options, parse_edge_filter_input,
    parse_per_mod_thresholds, parse_thresholds, using_stream,
};
use bio::io::fasta::Reader as FastaReader;
use clap::Args;
use crossbeam_channel::{bounded, Sender};
use derive_new::new;
use indicatif::{MultiProgress, ParallelProgressIterator, ProgressIterator};
use itertools::Itertools;
use log::{debug, error, info};
use rayon::prelude::*;
use rayon::{ThreadPool, ThreadPoolBuilder};
use rust_htslib::bam::{self, FetchDefinition, Read};
use rustc_hash::FxHashMap;

use crate::errs::RunError;
use crate::extract::writer::{OutwriterWithMemory, TsvWriterWithContigNames};
use crate::interval_chunks::IntervalChunks;
use crate::logging::init_logging;
use crate::mod_bam::{
    BaseModCall, BaseModProbs, CollapseMethod, EdgeFilter,
    TrackingModRecordIter,
};
use crate::mod_base_code::{DnaBase, ModCodeRepr};
use crate::monoid::Moniod;
use crate::motif_bed::{find_motif_hits, RegexMotif};
use crate::position_filter::{GenomeLapper, Iv, StrandedPositionFilter};
use crate::read_ids_to_base_mod_probs::{
    ModProfile, ReadBaseModProfile, ReadsBaseModProfile,
};
use crate::reads_sampler::record_sampler::RecordSampler;
use crate::reads_sampler::sample_reads_from_interval;
use crate::reads_sampler::sampling_schedule::SamplingSchedule;
use crate::record_processor::WithRecords;
use crate::threshold_mod_caller::MultipleThresholdModCaller;
use crate::util::{
    create_out_directory, get_master_progress_bar, get_reference_mod_strand,
    get_spinner, get_subroutine_progress_bar, get_targets, get_ticker, Kmer,
    ReferenceRecord, Region, Strand,
};
use crate::writers::TsvWriter;

#[derive(Args)]
pub struct ExtractMods {
    /// Path to modBAM file to extract read-level information from, or one of `-` or
    /// `stdin` to specify a stream from standard input. If a file is used it may
    /// be sorted and have associated index.
    in_bam: String,
    /// Path to output file, "stdout" or "-" will direct output to standard out.
    out_path: String,
    /// Number of threads to use
    #[arg(short = 't', long, default_value_t = 4)]
    threads: usize,
    /// Path to file to write run log.
    #[arg(long, alias = "log")]
    log_filepath: Option<PathBuf>,
    /// Include only mapped bases in output (alias: mapped).
    #[arg(long, alias = "mapped", default_value_t = false)]
    mapped_only: bool,
    /// Number of reads to use. Note that when using a sorted, indexed modBAM that
    /// the sampling algorithm will attempt to sample records evenly over the length
    /// of the reference sequence. The result is the final number of records used
    /// may be slightly more or less than the requested number. When piping from stdin
    /// or using a modBAM without an index, the requested number of reads will be exact.
    #[arg(long)]
    num_reads: Option<usize>,
    /// Process only reads that are aligned to a specified region of the BAM.
    /// Format should be <chrom_name>:<start>-<end> or <chrom_name>.
    #[arg(long)]
    region: Option<String>,
    /// Force overwrite of output file
    #[arg(long, default_value_t = false)]
    force: bool,
    /// Hide the progress bar.
    #[arg(long, default_value_t = false, hide_short_help = true)]
    suppress_progress: bool,
    /// Set the query and reference k-mer size (if a reference is provided). Maxumum number
    /// for this value is 12.
    #[arg(long, default_value_t = 5)]
    kmer_size: usize,
    /// Ignore the BAM index (if it exists) and default to a serial scan of the BAM.
    #[arg(long, default_value_t = false, hide_short_help = true)]
    ignore_index: bool,
    #[arg(long, alias = "read-calls", hide_short_help = true)]
    read_calls_path: Option<PathBuf>,
    #[arg(long, alias = "position-pileup", hide_short_help = true)]
    read_pileup_path: Option<PathBuf>,

    /// Path to reference FASTA to extract reference context information from.
    /// If no reference is provided, `ref_kmer` column will be "." in the output.
    /// (alias: ref)
    #[arg(long, alias = "ref")]
    reference: Option<PathBuf>,

    /// BED file with regions to include (alias: include-positions). Implicitly
    /// only includes mapped sites.
    #[arg(long, alias = "include-positions")]
    include_bed: Option<PathBuf>,
    /// BED file with regions to _exclude_ (alias: exclude).
    #[arg(long, alias = "exclude", short = 'v')]
    exclude_bed: Option<PathBuf>,
    /// Output read-level base modification probabilities restricted to the reference sequence
    /// motifs provided. The first argument should be the sequence motif and the second argument
    /// is the 0-based offset to the base to pileup base modification counts for.
    /// For example: --motif CGCG 0 indicates include base modifications for which the read is
    /// aligned to the first C on the top strand and the last C (complement to G) on
    /// the bottom strand. The --cpg argument is short hand for --motif CG 0.
    /// This argument can be passed multiple times.
    #[arg(long, action = clap::ArgAction::Append, num_args = 2, requires = "reference")]
    motif: Option<Vec<String>>,
    /// Only output counts at CpG motifs. Requires a reference sequence to be
    /// provided.
    #[arg(long, requires = "reference", default_value_t = false)]
    cpg: bool,
    /// When using motifs, respect soft masking in the reference sequence.
    #[arg(
        long,
        short = 'k',
        requires = "motif",
        default_value_t = false,
        hide_short_help = true
    )]
    mask: bool,

    // sampling and filtering
    /// Specify the filter threshold globally or per-base. Global filter threshold
    /// can be specified with by a decimal number (e.g. 0.75). Per-base thresholds
    /// can be specified by colon-separated values, for example C:0.75 specifies a
    /// threshold value of 0.75 for cytosine modification calls. Additional
    /// per-base thresholds can be specified by repeating the option: for example
    /// --filter-threshold C:0.75 --filter-threshold A:0.70 or specify a single
    /// base option and a default for all other bases with:
    /// --filter-threshold A:0.70 --filter-threshold 0.9 will specify a threshold
    /// value of 0.70 for adenine and 0.9 for all other base modification calls.
    #[arg(
        long,
        group = "thresholds",
        action = clap::ArgAction::Append,
        alias = "pass_threshold"
    )]
    filter_threshold: Option<Vec<String>>,
    /// Specify a passing threshold to use for a base modification, independent of the
    /// threshold for the primary sequence base or the default. For example, to set
    /// the pass threshold for 5hmC to 0.8 use `--mod-threshold h:0.8`. The pass
    /// threshold will still be estimated as usual and used for canonical cytosine and
    /// other modifications unless the `--filter-threshold` option is also passed.
    /// See the online documentation for more details.
    #[arg(
        long,
        alias = "mod-threshold",
        action = clap::ArgAction::Append,
        hide_short_help = true
    )]
    mod_thresholds: Option<Vec<String>>,
    /// Do not perform any filtering, include all mod base calls in output. See
    /// filtering.md for details on filtering.
    #[arg(
        conflicts_with_all = ["mod_thresholds", "filter_threshold"],
        long,
        default_value_t = false,
        hide_short_help = true
    )]
    no_filtering: bool,
    /// Interval chunk size in base pairs to process concurrently when estimating the threshold
    /// probability.
    #[arg(long, default_value_t = 1_000_000, hide_short_help = true)]
    sampling_interval_size: u32,
    /// Sample this fraction of the reads when estimating the pass-threshold.
    /// In practice, 10-100 thousand reads is sufficient to estimate the model output
    /// distribution and determine the filtering threshold. See filtering.md for
    /// details on filtering.
    #[arg(
        group = "sampling_options",
        short = 'f',
        long,
        hide_short_help = true
    )]
    sampling_frac: Option<f64>,
    /// Sample this many reads when estimating the filtering threshold. If a sorted, indexed modBAM
    /// is provided reads will be sampled evenly across aligned genome. If a region is specified,
    /// with the --region, then reads will be sampled evenly across the region given.
    /// This option is useful for large BAM files. In practice, 10-50 thousand reads is sufficient
    /// to estimate the model output distribution and determine the filtering threshold.
    #[arg(
        group = "sampling_options",
        short = 'n',
        long,
        default_value_t = 10_042
    )]
    sample_num_reads: usize,
    /// Set a random seed for deterministic running, the default is non-deterministic.
    #[arg(
        long,
        conflicts_with = "num_reads",
        requires = "sampling_frac",
        hide_short_help = true
    )]
    seed: Option<u64>,
    /// Filter out modified base calls where the probability of the predicted
    /// variant is below this confidence percentile. For example, 0.1 will filter
    /// out the 10% lowest confidence modification calls.
    #[arg(
        group = "thresholds",
        short = 'p',
        long,
        default_value_t = 0.1,
        hide_short_help = true
    )]
    filter_percentile: f32,

    /// Discard base modification calls that are this many bases from the start or the end
    /// of the read. Two comma-separated values may be provided to asymmetrically filter out
    /// base modification calls from the start and end of the reads. For example, 4,8 will
    /// filter out base modification calls in the first 4 and last 8 bases of the read.
    #[arg(long)]
    edge_filter: Option<String>,
    /// Invert the edge filter, instead of filtering out base modification calls at the ends
    /// of reads, only _keep_ base modification calls at the ends of reads. E.g. if usually,
    /// "4,8" would remove (i.e. filter out) base modification calls in the first 4 and last 8
    /// bases of the read, using this flag will keep only base modification calls in the first
    /// 4 and last 8 bases.
    #[arg(long, requires = "edge_filter", default_value_t = false)]
    invert_edge_filter: bool,

    /// Ignore a modified base class  _in_situ_ by redistributing base modification
    /// probability equally across other options. For example, if collapsing 'h',
    /// with 'm' and canonical options, half of the probability of 'h' will be added to
    /// both 'm' and 'C'. A full description of the methods can be found in
    /// collapse.md.
    #[arg(long, hide_short_help = true)]
    ignore: Option<String>,

    /// Interval chunk size in base pairs to process concurrently. Smaller interval
    /// chunk sizes will use less memory but incur more overhead. Only used when an
    /// indexed modBAM is provided.
    #[arg(
        short = 'i',
        long,
        default_value_t = 100_000,
        hide_short_help = true
    )]
    interval_size: u32,

    /// Ignore implicitly canonical base modification calls. When the `.`
    /// flag is used in the MM tag, this implies that bases missing a base
    /// modification probability are to be assumed canonical. Set this flag
    /// to omit those base modifications from the output. For additional
    /// details see the SAM spec: https://samtools.github.io/hts-specs/SAMtags.pdf.
    #[arg(long, hide_short_help = true)]
    ignore_implicit: bool,
}

type ReferenceAndIntervals = Vec<(ReferenceRecord, IntervalChunks)>;

impl ExtractMods {
    fn using_stdin(&self) -> bool {
        using_stream(&self.in_bam)
    }

    fn load_regions(
        &self,
        name_to_tid: &HashMap<&str, u32>,
        region: Option<&Region>,
        contigs: &HashMap<String, Vec<u8>>,
        master_progress_bar: &MultiProgress,
        thread_pool: &ThreadPool,
    ) -> anyhow::Result<(Option<ReferenceAndIntervals>, ReferencePositionFilter)>
    {
        let include_unmapped = if self.include_bed.is_some() {
            info!("specifying include-only BED outputs only mapped sites");
            false
        } else if self.motif.is_some() || self.cpg {
            info!("specifying a motif (including --cpg) outputs only mapped sites");
            false
        } else {
            !self.mapped_only
        };

        let motifs = if let Some(raw_motif_parts) = &self.motif {
            Some(RegexMotif::from_raw_parts(&raw_motif_parts, self.cpg)?)
        } else if self.cpg {
            Some(vec![RegexMotif::parse_string("CG", 0).unwrap()])
        } else {
            None
        };

        let include_positions = self
            .include_bed
            .as_ref()
            .map(|fp| {
                StrandedPositionFilter::from_bed_file(
                    fp,
                    name_to_tid,
                    self.suppress_progress,
                )
            })
            .transpose()?;

        let exclude_positions = self
            .exclude_bed
            .as_ref()
            .map(|fp| {
                StrandedPositionFilter::from_bed_file(
                    fp,
                    name_to_tid,
                    self.suppress_progress,
                )
            })
            .transpose()?;

        // intersect the motif positions with the include positions from the BED file
        let include_positions = if let Some(motifs) = motifs {
            let pb = master_progress_bar
                .add(get_subroutine_progress_bar(contigs.len()));
            pb.set_message("contigs searched");
            let tid_to_positions = thread_pool.install(|| {
                contigs
                    .into_par_iter()
                    .progress_with(pb)
                    .filter_map(|(name, raw_seq)| {
                        name_to_tid
                            .get(name.as_str())
                            .map(|tid| (*tid, raw_seq))
                    })
                    .map(|(tid, raw_seq)| {
                        let seq = raw_seq
                            .iter()
                            .map(|&b| b as char)
                            .collect::<String>();
                        let seq = if self.mask {
                            seq
                        } else {
                            seq.to_ascii_uppercase()
                        };
                        motifs
                            .par_iter()
                            .map(|motif| {
                                let positions = find_motif_hits(&seq, motif);
                                let positions = if let Some(filter) =
                                    include_positions.as_ref()
                                {
                                    positions
                                        .into_iter()
                                        .filter(|(pos, strand)| {
                                            filter.contains(
                                                tid as i32,
                                                *pos as u64,
                                                *strand,
                                            )
                                        })
                                        .collect::<Vec<(usize, Strand)>>()
                                } else {
                                    positions
                                };
                                (tid, positions)
                            })
                            .collect::<HashMap<u32, Vec<(usize, Strand)>>>()
                    })
                    .reduce(|| HashMap::zero(), |a, b| a.op(b))
            });
            let (pos_lappers, neg_lappers) = tid_to_positions.into_iter().fold(
                (FxHashMap::default(), FxHashMap::default()),
                |(mut pos, mut neg), (tid, positions)| {
                    let to_lapper =
                        |intervals: Vec<(Iv, Strand)>| -> GenomeLapper<()> {
                            let intervals = intervals
                                .into_iter()
                                .map(|(iv, _)| iv)
                                .collect();
                            GenomeLapper::new(intervals)
                        };

                    let (pos_positions, neg_positions): (
                        Vec<(Iv, Strand)>,
                        Vec<(Iv, Strand)>,
                    ) = positions
                        .into_iter()
                        .map(|(position, strand)| {
                            let iv = Iv {
                                start: position as u64,
                                stop: (position + 1) as u64,
                                val: (),
                            };
                            (iv, strand)
                        })
                        .partition(|(_iv, strand)| *strand == Strand::Positive);
                    let pos_lapper = to_lapper(pos_positions);
                    let neg_lapper = to_lapper(neg_positions);
                    pos.insert(tid, pos_lapper);
                    neg.insert(tid, neg_lapper);
                    (pos, neg)
                },
            );

            Some(StrandedPositionFilter {
                pos_positions: pos_lappers,
                neg_positions: neg_lappers,
            })
        } else {
            include_positions
        };

        let reference_and_intervals = if !self.using_stdin()
            && !self.ignore_index
        {
            match bam::IndexedReader::from_path(&self.in_bam) {
                Ok(reader) => {
                    info!("found BAM index, processing reads in {} base pair chunks", self.interval_size);
                    let reference_records =
                        get_targets(reader.header(), region);
                    let reference_and_intervals = reference_records
                        .into_iter()
                        .map(|reference_record| {
                            let interval_chunks =
                                IntervalChunks::new_without_motifs(
                                    reference_record.start,
                                    reference_record.length,
                                    self.interval_size,
                                    reference_record.tid,
                                );
                            (reference_record, interval_chunks)
                        })
                        .collect::<ReferenceAndIntervals>();
                    Some(reference_and_intervals)
                }
                Err(_) => {
                    info!(
                    "did not find index to modBAM, defaulting to serial scan"
                );
                    None
                }
            }
        } else {
            None
        };

        let reference_position_filter = ReferencePositionFilter::new(
            include_positions,
            exclude_positions,
            include_unmapped,
        );

        Ok((reference_and_intervals, reference_position_filter))
    }

    pub(crate) fn run(&self) -> anyhow::Result<()> {
        let _handle = init_logging(self.log_filepath.as_ref());

        if self.kmer_size > 12 {
            bail!("kmer size must be less than or equal to 12")
        }

        let pool =
            ThreadPoolBuilder::new().num_threads(self.threads).build()?;

        let collapse_method = match &self.ignore {
            Some(raw_mod_code) => {
                let mod_code = ModCodeRepr::parse(raw_mod_code)?;
                Some(CollapseMethod::ReDistribute(mod_code))
            }
            None => None,
        };
        let edge_filter = self
            .edge_filter
            .as_ref()
            .map(|raw| parse_edge_filter_input(raw, self.invert_edge_filter))
            .transpose()?;

        let mut reader = get_serial_reader(&self.in_bam)?;
        let header = reader.header().to_owned();

        let (snd, rcv) = bounded(100_000);

        let tid_to_name = (0..header.target_count())
            .filter_map(|tid| {
                match String::from_utf8(header.tid2name(tid).to_vec()) {
                    Ok(contig) => Some((tid, contig)),
                    Err(e) => {
                        error!(
                            "failed to parse contig {tid}, {}",
                            e.to_string()
                        );
                        None
                    }
                }
            })
            .collect::<HashMap<u32, String>>();
        let name_to_tid = tid_to_name
            .iter()
            .map(|(tid, name)| (name.as_str(), *tid))
            .collect::<HashMap<&str, u32>>();

        let chrom_to_seq = match self.reference.as_ref() {
            Some(fp) => {
                let reader = FastaReader::from_file(fp)?;
                let pb = get_spinner();
                pb.set_message("parsing FASTA records");
                reader
                    .records()
                    .progress_with(pb)
                    .filter_map(|r| r.ok())
                    .filter(|record| name_to_tid.get(record.id()).is_some())
                    .map(|record| {
                        (record.id().to_owned(), record.seq().to_vec())
                    })
                    .collect::<HashMap<String, Vec<u8>>>()
            }
            None => HashMap::new(),
        };

        let multi_prog = MultiProgress::new();
        if self.suppress_progress {
            multi_prog.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        }

        let region = self
            .region
            .as_ref()
            .map(|raw_region| Region::parse_str(raw_region, &header))
            .transpose()?;

        let per_mod_thresholds = self
            .mod_thresholds
            .as_ref()
            .map(|raw_per_mod_thresholds| {
                parse_per_mod_thresholds(raw_per_mod_thresholds)
            })
            .transpose()?;

        let (references_and_intervals, reference_position_filter) = self
            .load_regions(
                &name_to_tid,
                region.as_ref(),
                &chrom_to_seq,
                &multi_prog,
                &pool,
            )?;

        let caller = if self.read_calls_path.is_some()
            || self.read_pileup_path.is_some()
        {
            if self.no_filtering {
                // need this here because input can be stdin
                MultipleThresholdModCaller::new_passthrough()
            } else {
                // stdin input and want a threshold, not allowed
                if self.using_stdin() && self.filter_threshold.is_none() {
                    bail!("\
                        cannot use stdin and estimate a filter threshold, set the threshold on the \
                        command line with --filter-threshold and/or --mod-threshold (or set \
                        --no-filtering).")
                }
                if let Some(raw_threshold) = &self.filter_threshold {
                    parse_thresholds(raw_threshold, per_mod_thresholds)?
                } else {
                    let in_bam = Path::new(&self.in_bam).to_path_buf();
                    if !in_bam.exists() {
                        bail!(
                            "failed to find input modBAM file at {}",
                            self.in_bam
                        );
                    }
                    pool.install(|| {
                        get_threshold_from_options(
                            &in_bam,
                            self.threads,
                            self.sampling_interval_size,
                            self.sampling_frac,
                            self.sample_num_reads,
                            false,
                            self.filter_percentile,
                            self.seed,
                            region.as_ref(),
                            per_mod_thresholds,
                            edge_filter.as_ref(),
                            collapse_method.as_ref(),
                            reference_position_filter.include_pos.as_ref(),
                            !reference_position_filter.include_unmapped,
                            self.suppress_progress,
                        )
                    })?
                }
            }
        } else {
            MultipleThresholdModCaller::new_passthrough()
        };

        // allowed to use the sampling schedule if there is an index, if
        // asked for num_reads with no index, scan first N reads
        let schedule = match (self.num_reads, self.using_stdin()) {
            (_, true) => None,
            (Some(num_reads), false) => {
                match bam::IndexedReader::from_path(self.in_bam.as_str()) {
                    Ok(_) => Some(SamplingSchedule::from_num_reads(
                        &self.in_bam,
                        num_reads,
                        region.as_ref(),
                        reference_position_filter.include_pos.as_ref(),
                        reference_position_filter.include_unmapped,
                    )?),
                    Err(_) => {
                        debug!("cannot use sampling schedule without index, keeping first {num_reads} reads");
                        None
                    }
                }
            }
            (None, false) => None,
        };

        let n_failed = multi_prog.add(get_ticker());
        n_failed.set_message("~records failed");
        let n_skipped = multi_prog.add(get_ticker());
        n_skipped.set_message("~records skipped");
        let n_used = multi_prog.add(get_ticker());
        n_used.set_message("~records used");
        let n_rows = multi_prog.add(get_ticker());
        n_rows.set_message("rows written");
        reader.set_threads(self.threads)?;
        let n_reads = self.num_reads;
        let threads = self.threads;
        let mapped_only = self.mapped_only;
        let in_bam = self.in_bam.clone();
        let kmer_size = self.kmer_size;

        thread::spawn(move || {
            pool.install(|| {
                // references_and_intervals is only some when we have an index
                if let Some(reference_and_intervals) = references_and_intervals {
                    drop(reader);
                    // should make this a method on this struct?
                    let bam_fp = Path::new(&in_bam).to_path_buf();

                    // if using unmapped add 1 to total chrms to traverse
                    let prog_length = if reference_position_filter.include_unmapped &&
                        schedule.as_ref().map(|s| s.has_unmapped()).unwrap_or(true) {
                        reference_and_intervals.len() + 1
                    } else {
                        reference_and_intervals.len()
                    };
                    let master_progress = multi_prog.add(get_master_progress_bar(prog_length));
                    master_progress.set_message("contigs");

                    let mut num_aligned_reads_used = 0usize;
                    for (reference_record, interval_chunks) in reference_and_intervals {
                        let interval_chunks =
                            interval_chunks
                                .filter(|(start, end)| {
                                    reference_position_filter.include_pos
                                        .as_ref()
                                        .map(|pf| {
                                            pf.overlaps_not_stranded(
                                                reference_record.tid,
                                                *start as u64,
                                                *end as u64
                                            )
                                        })
                                        .unwrap_or(true)
                                })
                                .collect::<Vec<(u32, u32)>>();

                        let total_interval_length = interval_chunks
                            .iter()
                            .map(|(start, end)| end.checked_sub(*start).unwrap_or(0))
                            .sum::<u32>();

                        // skip this contig if there aren't any reads
                        let ref_has_reads = schedule
                            .as_ref()
                            .map(|s| s.chrom_has_reads(reference_record.tid))
                            .unwrap_or(true);
                        if !ref_has_reads {
                            master_progress.inc(1);
                            continue
                        }

                        let interval_pb = multi_prog.add(get_subroutine_progress_bar(interval_chunks.len()));
                        interval_pb.set_message(format!("processing {}", &reference_record.name));
                        let n_reads_used = interval_chunks.into_par_iter()
                            .progress_with(interval_pb)
                            .map(
                                |(start, end)| {
                                    let record_sampler = schedule.as_ref()
                                        .map(|sampling_schedule| {
                                            sampling_schedule.get_record_sampler(&reference_record, total_interval_length, start, end)
                                    }).unwrap_or(RecordSampler::new_passthrough());

                                    let batch_result = sample_reads_from_interval::<
                                        ReadsBaseModProfile,
                                    >(
                                        &bam_fp,
                                        reference_record.tid,
                                        start,
                                        end,
                                        record_sampler,
                                        collapse_method.as_ref(),
                                        edge_filter.as_ref(),
                                        None,
                                        false,
                                        Some(kmer_size),
                                    ).map(|reads_base_mod_profile| {
                                        reference_position_filter.filter_read_base_mod_probs(reads_base_mod_profile)
                                    });
                                    let num_reads_success = batch_result.as_ref().map(|batch| batch.num_reads()).unwrap_or(0);

                                    match snd.send(batch_result) {
                                        Ok(_) => {
                                            num_reads_success
                                        }
                                        Err(e) => {
                                            error!( "failed to send result to writer, {}", e.to_string() );
                                            0
                                        }
                                    }
                                }
                            ).sum::<usize>();
                        num_aligned_reads_used += n_reads_used;
                        master_progress.inc(1);
                    }

                    if reference_position_filter.include_unmapped {
                        let n_unmapped_reads = n_reads.map(|nr| {
                            nr.checked_sub(num_aligned_reads_used).unwrap_or(0)
                        });
                        if let Some(n) = n_unmapped_reads {
                            debug!("processing {n} unmapped reads");
                        } else {
                            debug!("processing unmapped reads");
                        }
                        let reader = bam::IndexedReader::from_path(&bam_fp)
                            .and_then(|mut reader| reader.fetch(FetchDefinition::Unmapped).map(|_| reader))
                            .and_then(|mut reader| reader.set_threads(threads).map(|_| reader));
                        match reader {
                            Ok(mut reader) => {
                                let (skip, fail) = Self::process_records_to_chan(
                                    reader.records(),
                                    &multi_prog,
                                    &reference_position_filter,
                                    snd.clone(),
                                    n_unmapped_reads,
                                    collapse_method.as_ref(),
                                    edge_filter.as_ref(),
                                    false,
                                    "unmapped ",
                                        kmer_size,
                                );
                                let _ = snd.send(Ok(ReadsBaseModProfile::new(Vec::new(), skip, fail)));
                            },
                            Err(e) => {
                                error!("failed to get indexed reader for unmapped read processing, {}", e.to_string());
                            }
                        }
                    }
                } else {
                    let (skip, fail) = Self::process_records_to_chan(
                        reader.records(),
                        &multi_prog,
                        &reference_position_filter,
                        snd.clone(),
                        n_reads,
                        collapse_method.as_ref(),
                            edge_filter.as_ref(),
                            mapped_only,
                            "",
                        kmer_size,
                    );
                    let _ = snd.send(Ok(ReadsBaseModProfile::new(Vec::new(), skip, fail)));
                }
            })
        });

        let read_calls_writer = if let Some(fp) = self.read_calls_path.as_ref()
        {
            create_out_directory(fp)?;
            let fp = fp
                .to_str()
                .ok_or(anyhow!("{fp:?} is an invalid path for read calls"))?;
            Some(TsvWriter::new_file(
                fp,
                self.force,
                Some(PositionModCalls::header()),
            )?)
        } else {
            None
        };

        let mut writer: Box<dyn OutwriterWithMemory<ReadsBaseModProfile>> =
            match self.out_path.as_str() {
                "stdout" | "-" => {
                    let tsv_writer =
                        TsvWriter::new_stdout(Some(ModProfile::header()));
                    let writer = TsvWriterWithContigNames::new(
                        tsv_writer,
                        tid_to_name,
                        chrom_to_seq,
                        HashSet::new(),
                        read_calls_writer,
                        None, // todo pileup
                        caller,
                    );
                    Box::new(writer)
                }
                _ => {
                    let tsv_writer = TsvWriter::new_file(
                        &self.out_path,
                        self.force,
                        Some(ModProfile::header()),
                    )?;
                    let writer = TsvWriterWithContigNames::new(
                        tsv_writer,
                        tid_to_name,
                        chrom_to_seq,
                        HashSet::new(),
                        read_calls_writer,
                        None, // todo pileup
                        caller,
                    );
                    Box::new(writer)
                }
            };

        let remove_inferred = self.ignore_implicit;
        for result in rcv {
            match result {
                Ok(mod_profile) => {
                    let mod_profile = if remove_inferred {
                        mod_profile.remove_inferred()
                    } else {
                        mod_profile
                    };
                    n_used.inc(mod_profile.num_reads() as u64);
                    n_failed.inc(mod_profile.num_fails as u64);
                    n_skipped.inc(mod_profile.num_skips as u64);
                    match writer.write(mod_profile, kmer_size) {
                        Ok(n) => n_rows.inc(n),
                        Err(e) => {
                            error!("failed to write {}", e.to_string());
                        }
                    }
                }
                Err(e) => {
                    debug!(
                        "failed to calculate read-level mod probs, {}",
                        e.to_string()
                    );
                }
            }
        }
        n_failed.finish_and_clear();
        n_skipped.finish_and_clear();
        n_used.finish_and_clear();
        n_rows.finish_and_clear();
        info!(
            "processed {} reads, {} rows, skipped ~{} reads, failed ~{} reads",
            writer.num_reads(),
            n_rows.position(),
            n_skipped.position(),
            n_failed.position()
        );
        Ok(())
    }

    fn process_records_to_chan<'a, T: Read>(
        records: bam::Records<T>,
        multi_pb: &MultiProgress,
        reference_position_filter: &ReferencePositionFilter,
        snd: Sender<anyhow::Result<ReadsBaseModProfile>>,
        n_reads: Option<usize>,
        collapse_method: Option<&CollapseMethod>,
        edge_filter: Option<&EdgeFilter>,
        only_mapped: bool,
        message: &'static str,
        kmer_size: usize,
    ) -> (usize, usize) {
        let mut mod_iter = TrackingModRecordIter::new(records, false);
        let pb = multi_pb.add(get_spinner());
        pb.set_message(format!("{message}records processed"));
        for (record, read_id, mod_base_info) in &mut mod_iter {
            if record.is_unmapped() && only_mapped {
                continue;
            }
            let mod_profile = match ReadBaseModProfile::process_record(
                &record,
                &read_id,
                mod_base_info,
                collapse_method,
                edge_filter,
                kmer_size,
            ) {
                Ok(mod_profile) => {
                    ReadsBaseModProfile::new(vec![mod_profile], 0, 0)
                }
                Err(run_error) => match run_error {
                    RunError::BadInput(_) | RunError::Failed(_) => {
                        ReadsBaseModProfile::new(Vec::new(), 0, 1)
                    }
                    RunError::Skipped(_) => {
                        ReadsBaseModProfile::new(Vec::new(), 1, 0)
                    }
                },
            };
            let mod_profile = reference_position_filter
                .filter_read_base_mod_probs(mod_profile);
            match snd.send(Ok(mod_profile)) {
                Ok(_) => {
                    pb.inc(1);
                }
                Err(snd_error) => {
                    error!(
                        "failed to send results to writer, {}",
                        snd_error.to_string()
                    );
                }
            }
            let done = n_reads
                .map(|nr| pb.position() as usize >= nr)
                .unwrap_or(false);
            if done {
                debug!("stopping after processing {} reads", pb.position());
                break;
            }
        }
        pb.finish_and_clear();
        (mod_iter.num_skipped, mod_iter.num_failed)
    }
}

#[derive(new)]
struct ReferencePositionFilter {
    include_pos: Option<StrandedPositionFilter<()>>,
    exclude_pos: Option<StrandedPositionFilter<()>>,
    include_unmapped: bool,
}

impl ReferencePositionFilter {
    fn keep(
        &self,
        chrom_id: u32,
        position: u64,
        alignment_strand: Strand,
        mod_strand: Strand,
    ) -> bool {
        let reference_mod_strand =
            get_reference_mod_strand(mod_strand, alignment_strand);
        let include_hit = self
            .include_pos
            .as_ref()
            .map(|flt| {
                flt.contains(chrom_id as i32, position, reference_mod_strand)
            })
            .unwrap_or(true);
        let exclude_hit = self
            .exclude_pos
            .as_ref()
            .map(|filt| {
                filt.contains(chrom_id as i32, position, reference_mod_strand)
            })
            .unwrap_or(false);

        include_hit && !exclude_hit
    }

    fn filter_read_base_mod_probs(
        &self,
        reads_base_mods_profile: ReadsBaseModProfile,
    ) -> ReadsBaseModProfile {
        let mut n_skipped = reads_base_mods_profile.num_skips;
        let n_failed = reads_base_mods_profile.num_fails;
        let profiles = reads_base_mods_profile
            .profiles
            .into_par_iter()
            .map(|read_base_mod_profile| {
                let read_name = read_base_mod_profile.record_name;
                let chrom_id = read_base_mod_profile.chrom_id;
                let profile = read_base_mod_profile
                    .profile
                    .into_par_iter()
                    .filter(|mod_profile| {
                        match (
                            chrom_id,
                            mod_profile.ref_position,
                            mod_profile.alignment_strand,
                        ) {
                            (Some(chrom_id), Some(ref_pos), Some(strand)) => {
                                self.keep(
                                    chrom_id,
                                    ref_pos as u64,
                                    strand,
                                    mod_profile.mod_strand,
                                )
                            }
                            _ => self.include_unmapped,
                        }
                    })
                    .collect::<Vec<ModProfile>>();
                ReadBaseModProfile::new(read_name, chrom_id, profile)
            })
            .collect::<Vec<ReadBaseModProfile>>();
        let empty = profiles
            .iter()
            .filter(|read_base_mod_profile| {
                read_base_mod_profile.profile.is_empty()
            })
            .count();
        n_skipped += empty;
        ReadsBaseModProfile::new(profiles, n_skipped, n_failed)
    }
}

#[derive(new)]
pub(crate) struct PositionModCalls {
    query_position: usize,
    pub(crate) ref_position: Option<i64>,
    aligned_query_position: usize,
    num_soft_clipped_start: usize,
    num_soft_clipped_end: usize,
    base_mod_probs: BaseModProbs,
    q_base: u8,
    query_kmer: Kmer,
    pub(crate) mod_strand: Strand,
    pub(crate) alignment_strand: Option<Strand>,
    canonical_base: DnaBase,
}

impl PositionModCalls {
    fn header() -> String {
        let tab = '\t';
        format!(
            "\
            read_id{tab}\
            forward_read_position{tab}\
            forward_aligned_read_position{tab}\
            ref_position{tab}\
            chrom{tab}\
            mod_strand{tab}\
            ref_strand{tab}\
            ref_mod_strand{tab}\
            fw_soft_clipped_start{tab}\
            fw_soft_clipped_end{tab}\
            call_prob{tab}\
            call_code{tab}\
            base_qual{tab}\
            ref_kmer{tab}\
            query_kmer{tab}\
            canonical_base{tab}\
            modified_primary_base{tab}\
            filtered{tab}\
            inferred\n"
        )
    }

    pub(crate) fn from_profile(
        read_id: &str,
        profile: &[ModProfile],
    ) -> Vec<Self> {
        type Key = (usize, Strand, DnaBase);
        let mod_codes = profile
            .iter()
            .map(|x| x.raw_mod_code)
            .collect::<HashSet<ModCodeRepr>>()
            .into_iter()
            .collect::<Vec<ModCodeRepr>>();

        profile.iter()
            .fold(HashMap::<Key, Vec<&ModProfile>>::new(), |mut acc, x| {
                let k = (x.query_position, x.mod_strand, x.canonical_base);
                acc.entry(k).or_insert(Vec::new()).push(x);
                acc
            })
            .into_iter()
            .fold(Vec::<Self>::new(), |mut acc, ((query_pos, strand, base), mod_profile)| {
                let base_mod_probs = if mod_profile.iter().any(|x| x.inferred) {
                    if mod_profile.len() != 1 {
                        // todo come back and make this debug after testing
                        error!("should have only 1 when position is inferred? read: {read_id} pos: {query_pos}.");
                    }
                    BaseModProbs::new_inferred_canonical(&mod_codes)
                } else {
                    let mut probs = mod_profile
                        .iter()
                        .map(|x| {
                            (x.raw_mod_code, x.q_mod)
                        }).collect::<FxHashMap<ModCodeRepr, f32>>();
                    for code in mod_codes.iter() {
                        if !probs.contains_key(&code) {
                            probs.insert(*code, 0f32);
                        }
                    }

                    BaseModProbs::new(probs, false)
                };
                let template = &mod_profile[0];
                let ref_position = template.ref_position;
                let aligned_query_position = template
                    .query_position
                    .checked_sub(template.num_soft_clipped_start).unwrap_or(0);
                let num_clip_start = template.num_soft_clipped_start;
                let num_clip_end = template.num_soft_clipped_end;
                let q_base = template.q_base;
                let kmer = template.query_kmer;
                let alignment_strand = template.alignment_strand;


                let pos_mod_calls = PositionModCalls::new(
                    query_pos,
                    ref_position,
                    aligned_query_position,
                    num_clip_start,
                    num_clip_end,
                        base_mod_probs,
                    q_base,
                    kmer,
                    strand,
                    alignment_strand,
                    base
                );
                acc.push(pos_mod_calls);

                acc
        })
        .into_iter()
        .sorted_by(|a, b| {
            if a.alignment_strand.map(|s| s == Strand::Negative).unwrap_or(false) {
                b.query_position.cmp(&a.query_position)
            } else {
                a.query_position.cmp(&b.query_position)
            }
        }).collect()
    }

    pub(crate) fn to_row(
        &self,
        read_id: &str,
        chrom_name: Option<&String>,
        caller: &MultipleThresholdModCaller,
        reference_seqs: &HashMap<String, Vec<u8>>,
    ) -> String {
        let tab = '\t';
        let missing = ".".to_string();
        let chrom_name = chrom_name.unwrap_or(&missing).to_owned();
        let forward_read_position = self.query_position;
        let forward_aligned_read_position = self.aligned_query_position;
        let ref_position = self.ref_position.unwrap_or(-1);
        let mod_strand = self.mod_strand.to_char();
        let ref_strand =
            self.alignment_strand.map(|x| x.to_char()).unwrap_or('.');
        let ref_mod_strand = self
            .alignment_strand
            .map(|x| get_reference_mod_strand(self.mod_strand, x).to_char())
            .unwrap_or('.');
        let fw_soft_clipped_start = self.num_soft_clipped_start;
        let fw_soft_clipped_end = self.num_soft_clipped_end;
        let (mod_call_prob, mod_call_code) =
            match self.base_mod_probs.argmax_base_mod_call() {
                BaseModCall::Canonical(p) => (p, "-".to_string()),
                BaseModCall::Modified(p, code) => (p, code.to_string()),
                BaseModCall::Filtered => {
                    unreachable!("argmax should not output filtered calls")
                }
            };
        let base_qual = self.q_base;
        let query_kmer = format!("{}", self.query_kmer);
        let ref_kmer = if let Some(ref_pos) = self.ref_position {
            if ref_pos < 0 {
                ".".to_string()
            } else {
                reference_seqs
                    .get(&chrom_name)
                    .map(|s| {
                        Kmer::from_seq(
                            s,
                            ref_pos as usize,
                            self.query_kmer.size,
                        )
                        .to_string()
                    })
                    .unwrap_or(".".to_string())
            }
        } else {
            ".".to_string()
        };
        let canonical_base = self.canonical_base.char();
        let modified_primary_base = if self.mod_strand == Strand::Negative {
            self.canonical_base.complement().char()
        } else {
            self.canonical_base.char()
        };
        let filtered = caller.call(&self.canonical_base, &self.base_mod_probs)
            == BaseModCall::Filtered;
        let inferred = self.base_mod_probs.inferred;

        format!(
            "\
            {read_id}{tab}\
            {forward_read_position}{tab}\
            {forward_aligned_read_position}{tab}\
            {ref_position}{tab}\
            {chrom_name}{tab}\
            {mod_strand}{tab}\
            {ref_strand}{tab}\
            {ref_mod_strand}{tab}\
            {fw_soft_clipped_start}{tab}\
            {fw_soft_clipped_end}{tab}\
            {mod_call_prob}{tab}\
            {mod_call_code}{tab}\
            {base_qual}{tab}\
            {ref_kmer}{tab}\
            {query_kmer}{tab}\
            {canonical_base}{tab}\
            {modified_primary_base}{tab}\
            {filtered}{tab}\
            {inferred}\n"
        )
    }
}
