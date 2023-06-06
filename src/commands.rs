use std::collections::{HashMap, HashSet};
use std::num::ParseFloatError;
use std::path::PathBuf;
use std::thread;

use crate::adjust::{adjust_modbam, record_is_valid};
use crate::command_utils::{
    get_threshold_from_options, parse_per_mod_thresholds, parse_thresholds,
};
use anyhow::{anyhow, Context, Result as AnyhowResult};
use clap::{Args, Subcommand, ValueEnum};
use crossbeam_channel::bounded;
use histo_fp::Histogram;
use indicatif::{
    MultiProgress, ParallelProgressIterator, ProgressBar, ProgressStyle,
};
use log::{debug, error, info, warn};
use rayon::prelude::*;
use rust_htslib::bam;
use rust_htslib::bam::record::{Aux, AuxArray};
use rust_htslib::bam::Read;

use crate::errs::{InputError, RunError};
use crate::extract_mods::ExtractMods;
use crate::interval_chunks::IntervalChunks;
use crate::logging::init_logging;
use crate::mod_bam::{
    format_mm_ml_tag, CollapseMethod, EdgeFilter, ModBaseInfo, RawModCode,
    SkipMode, ML_TAGS, MM_TAGS,
};
use crate::mod_base_code::{DnaBase, ModCode, ParseChar};
use crate::motif_bed::{motif_bed, MotifLocations, RegexMotif};
use crate::pileup::{
    process_region, subcommand::ModBamPileup, ModBasePileup,
    PileupNumericOptions,
};
use crate::read_ids_to_base_mod_probs::ReadIdsToBaseModProbs;
use crate::reads_sampler::get_sampled_read_ids_to_base_mod_probs;
use crate::summarize::{summarize_modbam, ModSummary};
use crate::threshold_mod_caller::MultipleThresholdModCaller;
use crate::thresholds::{calc_threshold_from_bam, Percentiles};
use crate::util;
use crate::util::{add_modkit_pg_records, get_spinner, get_targets, Region};
use crate::writers::{
    BedGraphWriter, BedMethylWriter, MultiTableWriter, OutWriter, SampledProbs,
    TableWriter, TsvWriter,
};

#[derive(Subcommand)]
pub enum Commands {
    /// Tabulates base modification calls across genomic positions. This command
    /// produces a bedMethyl formatted file. Schema and description of fields can
    /// be found in the README.
    Pileup(ModBamPileup),
    /// Performs various operations on BAM files containing base modification
    /// information, such as converting base modification codes and ignoring
    /// modification calls. Produces a BAM output file.
    AdjustMods(Adjust),
    /// Renames Mm/Ml to tags to MM/ML. Also allows changing the the mode flag from
    /// silent '.' to explicitly '?' or '.'.
    UpdateTags(Update),
    /// Calculate an estimate of the base modification probability distribution.
    SampleProbs(SampleModBaseProbs),
    /// Summarize the mod tags present in a BAM and get basic statistics. The default
    /// output is a totals table (designated by '#' lines) and a modification calls
    /// table. Descriptions of the columns can be found in the README.
    Summary(ModSummarize),
    /// Call mods from a modbam, creates a new modbam with probabilities set to 100%
    /// if a base modification is called or 0% if called canonical.
    CallMods(CallMods),
    /// Create BED file with all locations of a sequence motif.
    /// Example: modkit motif-bed CG 0
    MotifBed(MotifBed),
    /// Extract read-level base modification information from a modBAM into a
    /// tab-separated values table.
    Extract(ExtractMods),
}

impl Commands {
    pub fn run(&self) -> Result<(), String> {
        match self {
            Self::AdjustMods(x) => x.run().map_err(|e| e.to_string()),
            Self::Pileup(x) => x.run().map_err(|e| e.to_string()),
            Self::SampleProbs(x) => x.run().map_err(|e| e.to_string()),
            Self::Summary(x) => x.run().map_err(|e| e.to_string()),
            Self::MotifBed(x) => x.run().map_err(|e| e.to_string()),
            Self::UpdateTags(x) => x.run(),
            Self::CallMods(x) => x.run().map_err(|e| e.to_string()),
            Self::Extract(x) => x.run().map_err(|e| e.to_string()),
        }
    }
}

type CliResult<T> = Result<T, RunError>;

fn get_sampling_options(
    no_sampling: bool,
    sampling_frac: Option<f64>,
    num_reads: usize,
) -> (Option<f64>, Option<usize>) {
    match (no_sampling, sampling_frac, num_reads) {
        // Both None tells RecordSampler to use passthrough
        // see `RecordSampler::new_from_options`
        (true, _, _) => {
            info!("not subsampling, using all reads");
            (None, None)
        }
        (false, Some(frac), _) => {
            let pct = frac * 100f64;
            info!("sampling {pct}% of reads");
            (sampling_frac, None)
        }
        (false, None, num_reads) => {
            info!("sampling {num_reads} reads from BAM");
            (None, Some(num_reads))
        }
    }
}

#[derive(Args)]
pub struct Adjust {
    /// BAM file to collapse mod call from.
    in_bam: PathBuf,
    /// File path to new BAM file to be created.
    out_bam: PathBuf,
    /// Output debug logs to file at this path.
    #[arg(long)]
    log_filepath: Option<PathBuf>,
    /// Modified base code to ignore/remove, see
    /// https://samtools.github.io/hts-specs/SAMtags.pdf for details on
    /// the modified base codes.
    #[arg(long, conflicts_with = "convert")]
    ignore: Option<char>,
    /// Number of threads to use.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// Fast fail, stop processing at the first invalid sequence record. Default
    /// behavior is to continue and report failed/skipped records at the end.
    #[arg(short, long = "ff", default_value_t = false)]
    fail_fast: bool,
    /// Convert one mod-tag to another, summing the probabilities together if
    /// the retained mod tag is already present.
    #[arg(group = "prob_args", long, action = clap::ArgAction::Append, num_args = 2)]
    convert: Option<Vec<char>>,
    /// Discard base modification calls that are this many bases from the start or the end
    /// of the read. For example, a value of 10 will require that the base modification is
    /// at least the 11th base or 11 bases from the end.
    #[arg(long)]
    edge_filter: Option<usize>,
}

impl Adjust {
    pub fn run(&self) -> AnyhowResult<()> {
        let _handle = init_logging(self.log_filepath.as_ref());
        let fp = &self.in_bam;
        let out_fp = &self.out_bam;
        let mut reader = bam::Reader::from_path(fp)?;
        let threads = self.threads;
        reader.set_threads(threads)?;
        let mut header = bam::Header::from_template(reader.header());
        add_modkit_pg_records(&mut header);
        let mut out_bam =
            bam::Writer::from_path(out_fp, &header, bam::Format::Bam)?;

        let methods = if let Some(convert) = &self.convert {
            let mut conversions = HashMap::new();
            for chunk in convert.chunks(2) {
                debug_assert_eq!(chunk.len(), 2);
                let from: RawModCode = chunk[0];
                let to: RawModCode = chunk[1];
                conversions.entry(to).or_insert(HashSet::new()).insert(from);
            }
            for (to_code, from_codes) in conversions.iter() {
                info!(
                    "Converting {} to {}",
                    from_codes.iter().collect::<String>(),
                    to_code
                )
            }
            conversions
                .into_iter()
                .map(|(to_mod_code, from_mod_codes)| {
                    let method = CollapseMethod::Convert {
                        to: to_mod_code,
                        from: from_mod_codes,
                    };

                    method
                })
                .collect::<Vec<CollapseMethod>>()
        } else {
            if let Some(ignore_base) = self.ignore.as_ref() {
                info!(
                    "Removing mod base {} from {}, new bam {}",
                    ignore_base,
                    fp.to_str().unwrap_or("???"),
                    out_fp.to_str().unwrap_or("???")
                );
                let method = CollapseMethod::ReDistribute(*ignore_base);
                vec![method]
            } else {
                Vec::new()
            }
        };

        let edge_filter = self
            .edge_filter
            .as_ref()
            .map(|trim| {
                info!("removing base modification calls from {trim} bases from the ends");
                EdgeFilter::new(*trim, *trim)
            });

        let methods = if edge_filter.is_none() && methods.is_empty() {
            warn!("no edge-filter, ignore, or convert was provided. Implicitly deciding to \
            perform ignore on modified base code h, this behavior will be removed in the next \
            release and will result in an error.");
            vec![CollapseMethod::ReDistribute('h')]
        } else {
            methods
        };

        adjust_modbam(
            &mut reader,
            &mut out_bam,
            &methods,
            None,
            edge_filter.as_ref(),
            self.fail_fast,
            "Adjusting modBAM",
        )?;
        Ok(())
    }
}

fn parse_percentiles(
    raw_percentiles: &str,
) -> Result<Vec<f32>, ParseFloatError> {
    if raw_percentiles.contains("..") {
        todo!("handle parsing ranges")
    } else {
        raw_percentiles
            .split(',')
            .map(|x| x.parse::<f32>())
            .collect()
    }
}

#[derive(Args)]
pub struct SampleModBaseProbs {
    /// Input BAM with modified base tags. If a index is found
    /// reads will be sampled evenly across the length of the
    /// reference sequence.
    in_bam: PathBuf,
    /// Number of threads to use.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// Specify a file for debug logs to be written to, otherwise ignore them.
    /// Setting a file is recommended.
    #[arg(long)]
    log_filepath: Option<PathBuf>,
    /// Hide the progress bar.
    #[arg(long, default_value_t = false, hide_short_help = true)]
    suppress_progress: bool,
    /// Percentiles to calculate, a space separated list of floats.
    #[arg(short, long, default_value_t=String::from("0.1,0.5,0.9"))]
    percentiles: String,
    /// Directory to deposit result tables into. Required for model probability
    /// histogram output. Creates two files probabilities.tsv and probabilities.txt
    /// The .txt contains ASCII-histograms and the .tsv contains tab-separated variable
    /// data represented by the histograms.
    #[arg(short = 'o', long)]
    out_dir: Option<PathBuf>,
    /// Label to prefix output files with. E.g. 'foo' will output
    /// foo_thresholds.tsv, foo_probabilities.tsv, and foo_probabilities.txt.
    #[arg(long, requires = "out_dir")]
    prefix: Option<String>,
    /// Overwrite results if present.
    #[arg(long, requires = "out_dir", default_value_t = false)]
    force: bool,
    /// Ignore a modified base class  _in_situ_ by redistributing base modification
    /// probability equally across other options. For example, if collapsing 'h',
    /// with 'm' and canonical options, half of the probability of 'h' will be added to
    /// both 'm' and 'C'. A full description of the methods can be found in
    /// collapse.md.
    #[arg(long, hide_short_help = true)]
    ignore: Option<char>,
    /// Discard base modification calls that are this many bases from the start or the end
    /// of the read. For example, a value of 10 will require that the base modification is
    /// at least the 11th base or 11 bases from the end.
    #[arg(long, hide_short_help = true)]
    edge_filter: Option<usize>,

    // probability histogram options
    /// Output histogram of base modification prediction probabilities.
    #[arg(long = "hist", requires = "out_dir", default_value_t = false)]
    histogram: bool,
    /// Number of buckets for the histogram, if used.
    #[arg(long, requires = "histogram", default_value_t = 128)]
    buckets: u64,

    /// Max number of reads to use, especially recommended when using a large
    /// BAM without an index. If an indexed BAM is provided, the reads will be
    /// sampled evenly over the length of the aligned reference. If a region is
    /// passed with the --region option, they will be sampled over the genomic
    /// region.
    #[arg(
        group = "sampling_options",
        short = 'n',
        long,
        default_value_t = 10_042
    )]
    num_reads: usize,
    /// Instead of using a defined number of reads, specify a fraction of reads
    /// to sample, for example 0.1 will sample 1/10th of the reads.
    #[arg(group = "sampling_options", short = 'f', long)]
    sampling_frac: Option<f64>,
    /// No sampling, use all of the reads to calculate the filter thresholds.
    #[arg(long, group = "sampling_options", default_value_t = false)]
    no_sampling: bool,
    /// Random seed for deterministic running, the default is non-deterministic.
    #[arg(short, requires = "sampling_frac", long)]
    seed: Option<u64>,

    /// Process only the specified region of the BAM when collecting probabilities.
    /// Format should be <chrom_name>:<start>-<end> or <chrom_name>.
    #[arg(long)]
    region: Option<String>,
    /// Interval chunk size in base pairs to process concurrently. Smaller interval
    /// chunk sizes will use less memory but incur more overhead. Only used when
    /// sampling probs from an indexed bam.
    #[arg(short = 'i', long, default_value_t = 1_000_000)]
    interval_size: u32,
}

impl SampleModBaseProbs {
    fn run(&self) -> AnyhowResult<()> {
        let _handle = init_logging(self.log_filepath.as_ref());
        let reader = bam::Reader::from_path(&self.in_bam)?;

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads)
            .build()?;

        let region = if let Some(raw_region) = &self.region {
            info!("parsing region {raw_region}");
            Some(Region::parse_str(raw_region, reader.header())?)
        } else {
            None
        };
        let edge_filter = self
            .edge_filter
            .as_ref()
            .map(|trim| EdgeFilter::new(*trim, *trim));

        let (sample_frac, num_reads) = get_sampling_options(
            self.no_sampling,
            self.sampling_frac,
            self.num_reads,
        );

        let collapse_method = if let Some(raw_mod_code_to_ignore) = self.ignore
        {
            let _ = ModCode::parse_raw_mod_code(raw_mod_code_to_ignore)?;
            Some(CollapseMethod::ReDistribute(raw_mod_code_to_ignore))
        } else {
            None
        };

        let desired_percentiles = parse_percentiles(&self.percentiles)
            .with_context(|| {
                format!("failed to parse percentiles: {}", &self.percentiles)
            })?;

        pool.install(|| {
            let read_ids_to_base_mod_calls =
                get_sampled_read_ids_to_base_mod_probs::<ReadIdsToBaseModProbs>(
                    &self.in_bam,
                    self.threads,
                    self.interval_size,
                    sample_frac,
                    num_reads,
                    self.seed,
                    region.as_ref(),
                    collapse_method.as_ref(),
                    edge_filter.as_ref(),
                    self.suppress_progress,
                )?;

            let histograms = if self.histogram {
                let mod_call_probs =
                    read_ids_to_base_mod_calls.mle_probs_per_base_mod();
                Some(
                    mod_call_probs
                        .iter()
                        .map(|(base, calls)| {
                            let mut hist =
                                Histogram::with_buckets(self.buckets, Some(0));
                            for prob in calls {
                                hist.add(*prob)
                            }
                            (*base, hist)
                        })
                        .collect::<HashMap<char, Histogram>>(),
                )
            } else {
                None
            };

            let percentiles = read_ids_to_base_mod_calls
                .mle_probs_per_base()
                .into_iter()
                .map(|(canonical_base, mut probs)| {
                    Percentiles::new(&mut probs, &desired_percentiles)
                        .with_context(|| {
                            format!(
                                "failed to calculate threshold for base {}",
                                canonical_base.char()
                            )
                        })
                        .map(|percs| (canonical_base.char(), percs))
                })
                .collect::<AnyhowResult<HashMap<char, Percentiles>>>()?;

            let sampled_probs =
                SampledProbs::new(histograms, percentiles, self.prefix.clone());

            let mut writer: Box<dyn OutWriter<SampledProbs>> =
                if let Some(p) = &self.out_dir {
                    sampled_probs.check_path(p, self.force)?;
                    Box::new(MultiTableWriter::new(p.clone()))
                } else {
                    Box::new(TsvWriter::new_stdout(None))
                };

            writer.write(sampled_probs)?;

            Ok(())
        })
    }
}

#[derive(Args)]
pub struct ModSummarize {
    /// Input modBam file.
    in_bam: PathBuf,
    /// Number of threads to use.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// Specify a file for debug logs to be written to, otherwise ignore them.
    /// Setting a file is recommended.
    #[arg(long)]
    log_filepath: Option<PathBuf>,
    /// Output summary as a tab-separated variables stdout instead of a table.
    #[arg(long = "tsv", default_value_t = false)]
    tsv_format: bool,
    /// Hide the progress bar.
    #[arg(long, default_value_t = false, hide_short_help = true)]
    suppress_progress: bool,

    // sampling options
    /// Max number of reads to use for estimating the filter threshold and
    /// generating the summary, especially recommended when using a large
    /// BAM without an index. If an indexed BAM is provided, the reads will
    /// be sampled evenly over the length of the aligned reference. If a
    /// region is passed with the --region option, they will be sampled
    /// over the genomic region.
    #[arg(
        group = "sampling_options",
        short = 'n',
        long,
        default_value_t = 10_042
    )]
    num_reads: usize,
    /// Instead of using a defined number of reads, specify a fraction of reads
    /// to sample when estimating the filter threshold. For example 0.1 will
    /// sample 1/10th of the reads.
    #[arg(group = "sampling_options", short = 'f', long)]
    sampling_frac: Option<f64>,
    /// No sampling, use all of the reads to calculate the filter thresholds and
    /// generating the summary.
    #[arg(long, group = "sampling_options", default_value_t = false)]
    no_sampling: bool,
    /// Sets a random seed for deterministic running (when using --sample-frac),
    /// the default is non-deterministic.
    #[arg(short, requires = "sampling_frac", long)]
    seed: Option<u64>,

    // threshold options
    /// Do not perform any filtering, include all base modification calls in the
    /// summary. See filtering.md for details on filtering.
    #[arg(group = "thresholds", long, default_value_t = false)]
    no_filtering: bool,
    /// Filter out modified base calls where the probability of the predicted
    /// variant is below this confidence percentile. For example, 0.1 will filter
    /// out the 10% lowest confidence base modification calls.
    #[arg(group = "thresholds", short = 'p', long, default_value_t = 0.1)]
    filter_percentile: f32,
    /// Specify the filter threshold globally or per-base. Global filter threshold
    /// can be specified with by a decimal number (e.g. 0.75). Per-base thresholds
    /// can be specified by colon-separated values, for example C:0.75 specifies a
    /// threshold value of 0.75 for cytosine modification calls. Additional
    /// per-base thresholds can be specified by repeating the option: for example
    /// --filter-threshold C:0.75 --filter-threshold A:0.70 or specify a single
    /// base option and a default for all other bases with:
    /// --filter-threshold A:0.70 --filter-threshold 0.9 will specify a threshold
    /// value of 0.70 for adenosine and 0.9 for all other base modification calls.
    #[arg(
        long,
        group = "thresholds",
        action = clap::ArgAction::Append
    )]
    filter_threshold: Option<Vec<String>>,
    /// Specify a passing threshold to use for a base modification, independent of the
    /// threshold for the primary sequence base or the default. For example, to set
    /// the pass threshold for 5hmC to 0.8 use `--mod-threshold h:0.8`. The pass
    /// threshold will still be estimated as usual and used for canonical cytosine and
    /// 5mC unless the `--filter-threshold` option is also passed. See the online
    /// documentation for more details.
    #[arg(
    long,
    action = clap::ArgAction::Append
    )]
    mod_thresholds: Option<Vec<String>>,
    /// Ignore a modified base class  _in_situ_ by redistributing base modification
    /// probability equally across other options. For example, if collapsing 'h',
    /// with 'm' and canonical options, half of the probability of 'h' will be added to
    /// both 'm' and 'C'. A full description of the methods can be found in
    /// collapse.md.
    #[arg(long, group = "combine_args", hide_short_help = true)]
    ignore: Option<char>,
    /// Discard base modification calls that are this many bases from the start or the end
    /// of the read. For example, a value of 10 will require that the base modification is
    /// at least the 11th base or 11 bases from the end.
    #[arg(long, hide_short_help = true)]
    edge_filter: Option<usize>,

    /// Process only the specified region of the BAM when collecting probabilities.
    /// Format should be <chrom_name>:<start>-<end> or <chrom_name>.
    #[arg(long)]
    region: Option<String>,
    /// When using regions, interval chunk size in base pairs to process concurrently.
    /// Smaller interval chunk sizes will use less memory but incur more
    /// overhead.
    #[arg(short = 'i', long, default_value_t = 1_000_000)]
    interval_size: u32,
}

impl ModSummarize {
    pub fn run(&self) -> AnyhowResult<()> {
        let _handle = init_logging(self.log_filepath.as_ref());
        let reader = bam::Reader::from_path(&self.in_bam)?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads)
            .build()?;

        let region = self
            .region
            .as_ref()
            .map(|raw_region| Region::parse_str(raw_region, reader.header()))
            .transpose()?;
        let edge_filter = self
            .edge_filter
            .as_ref()
            .map(|trim| EdgeFilter::new(*trim, *trim));

        let (sample_frac, num_reads) = get_sampling_options(
            self.no_sampling,
            self.sampling_frac,
            self.num_reads,
        );

        let per_mod_thresholds =
            if let Some(raw_per_mod_thresholds) = &self.mod_thresholds {
                Some(parse_per_mod_thresholds(raw_per_mod_thresholds)?)
            } else {
                None
            };

        let filter_thresholds =
            if let Some(raw_thresholds) = &self.filter_threshold {
                info!("parsing user defined thresholds");
                Some(parse_thresholds(
                    raw_thresholds,
                    per_mod_thresholds.clone(),
                )?)
            } else if self.no_filtering {
                info!("not performing filtering");
                Some(MultipleThresholdModCaller::new_passthrough())
            } else {
                None
            };

        let collapse_method = if let Some(raw_mod_code_to_ignore) = self.ignore
        {
            let _ = ModCode::parse_raw_mod_code(raw_mod_code_to_ignore)?;
            Some(CollapseMethod::ReDistribute(raw_mod_code_to_ignore))
        } else {
            None
        };

        let mod_summary = pool.install(|| {
            summarize_modbam(
                &self.in_bam,
                self.threads,
                self.interval_size,
                sample_frac,
                num_reads,
                self.seed,
                region.as_ref(),
                self.filter_percentile,
                filter_thresholds,
                per_mod_thresholds,
                collapse_method.as_ref(),
                edge_filter.as_ref(),
                self.suppress_progress,
            )
        })?;

        let mut writer: Box<dyn OutWriter<ModSummary>> = if self.tsv_format {
            Box::new(TsvWriter::new_stdout(None))
        } else {
            Box::new(TableWriter::new())
        };
        writer.write(mod_summary)?;
        Ok(())
    }
}

#[derive(Args)]
pub struct MotifBed {
    /// Input FASTA file
    fasta: PathBuf,
    /// Motif to search for within FASTA, e.g. CG
    motif: String,
    /// Offset within motif, e.g. 0
    offset: usize,
    /// Respect soft masking in the reference FASTA.
    #[arg(long, short = 'k', default_value_t = false)]
    mask: bool,
}

impl MotifBed {
    fn run(&self) -> AnyhowResult<()> {
        let _handle = init_logging(None);
        motif_bed(&self.fasta, &self.motif, self.offset, self.mask)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
#[allow(non_camel_case_types)]
enum ModMode {
    ambiguous,
    implicit,
}

impl ModMode {
    fn to_skip_mode(self) -> SkipMode {
        match self {
            Self::ambiguous => SkipMode::Ambiguous,
            Self::implicit => SkipMode::ProbModified,
        }
    }
}

#[derive(Args)]
pub struct Update {
    /// BAM file to update modified base tags in.
    in_bam: PathBuf,
    /// File path to new BAM file to be created.
    out_bam: PathBuf,
    /// Mode, change mode to this value, options {'ambiguous', 'implicit'}.
    /// See spec at: https://samtools.github.io/hts-specs/SAMtags.pdf.
    /// 'ambiguous' ('?') means residues without explicit modification
    /// probabilities will not be assumed canonical or modified. 'implicit'
    /// means residues without explicit modification probabilities are
    /// assumed to be canonical.
    #[arg(short, long, value_enum)]
    mode: Option<ModMode>,
    /// Number of threads to use.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// Output debug logs to file at this path.
    #[arg(long)]
    log_filepath: Option<PathBuf>,
}

fn update_mod_tags(
    mut record: bam::Record,
    new_mode: Option<SkipMode>,
) -> CliResult<bam::Record> {
    let _ok = record_is_valid(&record)?;
    let mod_base_info = ModBaseInfo::new_from_record(&record)?;
    let mm_style = mod_base_info.mm_style;
    let ml_style = mod_base_info.ml_style;

    let mut mm_agg = String::new();
    let mut ml_agg = Vec::new();

    let (converters, mod_prob_iter) = mod_base_info.into_iter_base_mod_probs();
    for (base, strand, mut seq_pos_mod_probs) in mod_prob_iter {
        let converter = converters.get(&base).unwrap();
        if let Some(mode) = new_mode {
            seq_pos_mod_probs.skip_mode = mode;
        }
        let (mm, mut ml) =
            format_mm_ml_tag(seq_pos_mod_probs, strand, converter);
        mm_agg.push_str(&mm);
        ml_agg.extend_from_slice(&mut ml);
    }
    record
        .remove_aux(mm_style.as_bytes())
        .expect("failed to remove MM tag");
    record
        .remove_aux(ml_style.as_bytes())
        .expect("failed to remove ML tag");
    let mm = Aux::String(&mm_agg);
    let ml_arr: AuxArray<u8> = {
        let sl = &ml_agg;
        sl.into()
    };
    let ml = Aux::ArrayU8(ml_arr);
    record
        .push_aux(MM_TAGS[0].as_bytes(), mm)
        .expect("failed to add MM tag");
    record
        .push_aux(ML_TAGS[0].as_bytes(), ml)
        .expect("failed to add ML tag");

    Ok(record)
}

impl Update {
    fn run(&self) -> Result<(), String> {
        let _handle = init_logging(self.log_filepath.as_ref());
        let fp = &self.in_bam;
        let out_fp = &self.out_bam;
        let threads = self.threads;
        let mut reader =
            bam::Reader::from_path(fp).map_err(|e| e.to_string())?;
        reader.set_threads(threads).map_err(|e| e.to_string())?;
        let mut header = bam::Header::from_template(reader.header());
        add_modkit_pg_records(&mut header);

        let mut out_bam =
            bam::Writer::from_path(out_fp, &header, bam::Format::Bam)
                .map_err(|e| e.to_string())?;
        let spinner = get_spinner();

        spinner.set_message("Updating ModBAM");
        let mut total = 0usize;
        let mut total_failed = 0usize;
        let mut total_skipped = 0usize;

        for (i, result) in reader.records().enumerate() {
            if let Ok(record) = result {
                let record_name = util::get_query_name_string(&record)
                    .unwrap_or("???".to_owned());
                match update_mod_tags(
                    record,
                    self.mode.map(|m| m.to_skip_mode()),
                ) {
                    Err(RunError::BadInput(InputError(err)))
                    | Err(RunError::Failed(err)) => {
                        debug!("read {} failed, {}", record_name, err);
                        total_failed += 1;
                    }
                    Err(RunError::Skipped(_reason)) => {
                        total_skipped += 1;
                    }
                    Ok(record) => {
                        if let Err(err) = out_bam.write(&record) {
                            debug!("failed to write {}", err);
                            total_failed += 1;
                        } else {
                            spinner.inc(1);
                            total = i;
                        }
                    }
                }
            } else {
                total_failed += 1;
            }
        }

        spinner.finish_and_clear();

        info!(
            "done, {} records processed, {} failed, {} skipped",
            total, total_failed, total_skipped
        );
        Ok(())
    }
}

#[derive(Args)]
pub struct CallMods {
    // running args
    /// Input BAM, may be sorted and have associated index available.
    in_bam: PathBuf,
    /// Output BAM filepath.
    out_bam: PathBuf,
    /// Specify a file for debug logs to be written to, otherwise ignore them.
    /// Setting a file is recommended.
    #[arg(long)]
    log_filepath: Option<PathBuf>,
    // /// Process only the specified region of the BAM when performing transformation.
    // /// Format should be <chrom_name>:<start>-<end> or <chrom_name>.
    // #[arg(long)] todo(arand)
    // region: Option<String>,
    /// Fast fail, stop processing at the first invalid sequence record. Default
    /// behavior is to continue and report failed/skipped records at the end.
    #[arg(long = "ff", default_value_t = false)]
    fail_fast: bool,
    /// Hide the progress bar.
    #[arg(long, default_value_t = false, hide_short_help = true)]
    suppress_progress: bool,

    // processing args
    /// Number of threads to use while processing chunks concurrently.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    // /// Interval chunk size to process concurrently. Smaller interval chunk
    // /// sizes will use less memory but incur more overhead. Only used when
    // /// provided an indexed BAM.
    // #[arg( todo(arand)
    // short = 'i',
    // long,
    // default_value_t = 100_000,
    // hide_short_help = true
    // )]
    // interval_size: u32,

    // sampling args
    /// Sample this many reads when estimating the filtering threshold. If alignments are
    /// present reads will be sampled evenly across aligned genome. If a region is
    /// specified, either with the --region option or the --sample-region option, then
    /// reads will be sampled evenly across the region given. This option is useful for
    /// large BAM files. In practice, 10-50 thousand reads is sufficient to estimate the
    /// model output distribution and determine the filtering threshold.
    #[arg(
        group = "sampling_options",
        short = 'n',
        long,
        default_value_t = 10_042
    )]
    num_reads: usize,
    /// Sample this fraction of the reads when estimating the filter-percentile.
    /// In practice, 50-100 thousand reads is sufficient to estimate the model output
    /// distribution and determine the filtering threshold. See filtering.md for
    /// details on filtering.
    #[arg(
        group = "sampling_options",
        short = 'f',
        long,
        hide_short_help = true
    )]
    sampling_frac: Option<f64>,
    /// Set a random seed for deterministic running, the default is non-deterministic.
    #[arg(
        long,
        conflicts_with = "num_reads",
        requires = "sampling_frac",
        hide_short_help = true
    )]
    seed: Option<u64>,
    /// Specify a region for sampling reads from when estimating the threshold probability.
    /// If this option is not provided, but --region is provided, the genomic interval
    /// passed to --region will be used.
    /// Format should be <chrom_name>:<start>-<end> or <chrom_name>.
    #[arg(long)]
    sample_region: Option<String>,
    /// Interval chunk size to process concurrently when estimating the threshold
    /// probability, can be larger than the pileup processing interval.
    #[arg(long, default_value_t = 1_000_000, hide_short_help = true)]
    sampling_interval_size: u32,

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
    /// Specify the filter threshold globally or per primary base. A global filter
    /// threshold can be specified with by a decimal number (e.g. 0.75). Per-base
    /// thresholds can be specified by colon-separated values, for example C:0.75
    /// specifies a threshold value of 0.75 for cytosine modification calls. Additional
    /// per-base thresholds can be specified by repeating the option: for example
    /// --filter-threshold C:0.75 --filter-threshold A:0.70 or specify a single
    /// base option and a default for all other bases with:
    /// --filter-threshold A:0.70 --filter-threshold 0.9 will specify a threshold
    /// value of 0.70 for adenosine and 0.9 for all other base modification calls.
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
    /// 5mC unless the `--filter-threshold` option is also passed. See the online
    /// documentation for more details.
    #[arg(
    long = "mod-threshold",
    action = clap::ArgAction::Append
    )]
    mod_thresholds: Option<Vec<String>>,
    /// Don't filter base modification calls, assign each base modification to the
    /// highest probability prediction.
    #[arg(long, default_value_t = false)]
    no_filtering: bool,
    /// Discard base modification calls that are this many bases from the start or the end
    /// of the read. For example, a value of 10 will require that the base modification is
    /// at least the 11th base or 11 bases from the end.
    #[arg(long, hide_short_help = true)]
    edge_filter: Option<usize>,
}

impl CallMods {
    pub fn run(&self) -> AnyhowResult<()> {
        let _handle = init_logging(self.log_filepath.as_ref());

        let mut reader = bam::Reader::from_path(&self.in_bam)?;
        let threads = self.threads;
        reader.set_threads(threads)?;
        let mut header = bam::Header::from_template(reader.header());
        add_modkit_pg_records(&mut header);
        let mut out_bam =
            bam::Writer::from_path(&self.out_bam, &header, bam::Format::Bam)?;
        let edge_filter = self
            .edge_filter
            .as_ref()
            .map(|trim| EdgeFilter::new(*trim, *trim));

        let per_mod_thresholds =
            if let Some(raw_per_mod_thresholds) = &self.mod_thresholds {
                Some(parse_per_mod_thresholds(raw_per_mod_thresholds)?)
            } else {
                None
            };

        let sampling_region = if let Some(raw_region) = &self.sample_region {
            info!("parsing sample region {raw_region}");
            Some(Region::parse_str(raw_region, &reader.header())?)
        } else {
            None
        };

        let caller = if let Some(raw_threshold) = &self.filter_threshold {
            parse_thresholds(raw_threshold, per_mod_thresholds)?
        } else {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(self.threads)
                .build()
                .with_context(|| "failed to make threadpool")?;
            pool.install(|| {
                get_threshold_from_options(
                    &self.in_bam,
                    self.threads,
                    self.sampling_interval_size,
                    self.sampling_frac,
                    self.num_reads,
                    self.no_filtering,
                    self.filter_percentile,
                    self.seed,
                    sampling_region.as_ref(),
                    per_mod_thresholds,
                    edge_filter.as_ref(),
                    None,
                    self.suppress_progress,
                )
            })?
        };

        adjust_modbam(
            &mut reader,
            &mut out_bam,
            &[],
            Some(&caller),
            edge_filter.as_ref(),
            self.fail_fast,
            "Calling Mods",
        )?;

        Ok(())
    }
}
