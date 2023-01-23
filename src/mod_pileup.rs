use crate::mod_bam::{BaseModCall, CollapseMethod};
use crate::mod_base_code::{DnaBase, ModCode};
use crate::read_cache::ReadCache;
use crate::util::{record_is_secondary, Strand};
use itertools::Itertools;
use log::debug;
use rust_htslib::bam;
use rust_htslib::bam::{FetchDefinition, Read};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Copy, Clone)]
enum Feature {
    Delete,
    Filtered,
    NoCall(DnaBase),
    ModCall(ModCode),
}

#[derive(Debug)]
pub struct PileupFeatureCounts {
    pub strand: Strand,
    pub filtered_coverage: u32,
    pub raw_mod_code: char,
    pub fraction_modified: f32,
    pub n_canonical: u32,
    pub n_modified: u32,
    pub n_other_modified: u32,
    pub n_delete: u32,
    pub n_filtered: u32,
    pub n_diff: u32,
    pub n_nocall: u32,
}

struct FeatureVector {
    // idx  strand  count
    //--------------------
    // 0   +  delete
    // 1   +  filtered
    // 2   +  n A
    // 3   +  n C
    // 4   +  n G
    // 5   +  n T
    // 6   +  n A - canonical
    // 7   +  n C - canonical
    // 8   +  n a
    // 9   +  n h
    // 10  +  n m
    // 11  -  delete
    // 12  -  filtered
    // 13  -  n A
    // 14  -  n C
    // 15  -  n G
    // 16  -  n T
    // 17  -  n A - canonical
    // 18  -  n C - canonical
    // 19  -  n a
    // 20  -  n h
    // 21  -  n m
    counts: [u32; 22],
}

impl FeatureVector {
    pub fn new() -> Self {
        Self { counts: [0u32; 22] }
    }

    pub fn add_feature(&mut self, strand: Strand, feature: Feature) {
        match (strand, feature) {
            (Strand::Positive, Feature::Delete) => {
                self.counts[0] = self.counts[0].saturating_add(1)
            }
            (Strand::Positive, Feature::Filtered) => {
                self.counts[1] = self.counts[1].saturating_add(1)
            }
            (Strand::Positive, Feature::NoCall(DnaBase::A)) => {
                self.counts[2] = self.counts[2].saturating_add(1)
            }
            (Strand::Positive, Feature::NoCall(DnaBase::C)) => {
                self.counts[3] = self.counts[3].saturating_add(1)
            }
            (Strand::Positive, Feature::NoCall(DnaBase::G)) => {
                self.counts[4] = self.counts[4].saturating_add(1)
            }
            (Strand::Positive, Feature::NoCall(DnaBase::T)) => {
                self.counts[5] = self.counts[5].saturating_add(1)
            }
            (Strand::Positive, Feature::ModCall(ModCode::A)) => {
                self.counts[6] = self.counts[6].saturating_add(1)
            }
            (Strand::Positive, Feature::ModCall(ModCode::C)) => {
                self.counts[7] = self.counts[7].saturating_add(1)
            }
            (Strand::Positive, Feature::ModCall(ModCode::a)) => {
                self.counts[8] = self.counts[8].saturating_add(1)
            }
            (Strand::Positive, Feature::ModCall(ModCode::h)) => {
                self.counts[9] = self.counts[9].saturating_add(1)
            }
            (Strand::Positive, Feature::ModCall(ModCode::m)) => {
                self.counts[10] = self.counts[10].saturating_add(1)
            }

            (Strand::Negative, Feature::Delete) => {
                self.counts[11] = self.counts[11].saturating_add(1)
            }
            (Strand::Negative, Feature::Filtered) => {
                self.counts[12] = self.counts[12].saturating_add(1)
            }
            (Strand::Negative, Feature::NoCall(DnaBase::A)) => {
                self.counts[13] = self.counts[13].saturating_add(1)
            }
            (Strand::Negative, Feature::NoCall(DnaBase::C)) => {
                self.counts[14] = self.counts[14].saturating_add(1)
            }
            (Strand::Negative, Feature::NoCall(DnaBase::G)) => {
                self.counts[15] = self.counts[15].saturating_add(1)
            }
            (Strand::Negative, Feature::NoCall(DnaBase::T)) => {
                self.counts[16] = self.counts[16].saturating_add(1)
            }
            (Strand::Negative, Feature::ModCall(ModCode::A)) => {
                self.counts[17] = self.counts[17].saturating_add(1)
            }
            (Strand::Negative, Feature::ModCall(ModCode::C)) => {
                self.counts[18] = self.counts[18].saturating_add(1)
            }
            (Strand::Negative, Feature::ModCall(ModCode::a)) => {
                self.counts[19] = self.counts[19].saturating_add(1)
            }
            (Strand::Negative, Feature::ModCall(ModCode::h)) => {
                self.counts[20] = self.counts[20].saturating_add(1)
            }
            (Strand::Negative, Feature::ModCall(ModCode::m)) => {
                self.counts[21] = self.counts[21].saturating_add(1)
            }

            (_, Feature::ModCall(ModCode::G)) => {}
            (_, Feature::ModCall(ModCode::T)) => {}
        }
    }

    fn add_pileup_counts(
        pileup_options: &PileupNumericOptions,
        counts: &mut Vec<PileupFeatureCounts>,
        observed_mods: &HashSet<ModCode>,
        strand: Strand,
        filtered_coverage: u32,
        n_h: u32,
        n_m: u32,
        n_canonical: u32,
        n_delete: u32,
        n_filtered: u32,
        n_diff: u32,
        n_nocall: u32,
    ) {
        match pileup_options {
            PileupNumericOptions::Passthrough
            | PileupNumericOptions::Collapse(_) => {
                for (mod_code, (n_modified, n_other_modified)) in
                    [(ModCode::h, (n_h, n_m)), (ModCode::m, (n_m, n_h))]
                {
                    if observed_mods.contains(&mod_code) {
                        let percent_modified =
                            n_modified as f32 / filtered_coverage as f32;
                        counts.push(PileupFeatureCounts {
                            strand,
                            filtered_coverage,
                            raw_mod_code: mod_code.char(),
                            fraction_modified: percent_modified,
                            n_canonical,
                            n_modified,
                            n_other_modified,
                            n_delete,
                            n_filtered,
                            n_diff,
                            n_nocall,
                        })
                    }
                }
            }
            PileupNumericOptions::Combine => {
                let n_modified = n_h + n_m;
                let percent_modified =
                    n_modified as f32 / filtered_coverage as f32;
                counts.push(PileupFeatureCounts {
                    strand,
                    filtered_coverage,
                    raw_mod_code: ModCode::C.char(),
                    fraction_modified: percent_modified,
                    n_canonical,
                    n_modified,
                    n_other_modified: 0,
                    n_delete,
                    n_filtered,
                    n_diff,
                    n_nocall,
                })
            }
        }
    }

    pub fn decode(
        self,
        observed_mods: &HashSet<ModCode>,
        pileup_options: &PileupNumericOptions,
    ) -> Vec<PileupFeatureCounts> {
        let mut counts = Vec::new();
        // there is mod info on the + strand
        let pos_strand_n_delete = self.counts[0];
        let pos_stand_n_filt = self.counts[1];
        let neg_strand_n_delete = self.counts[11];
        let neg_stand_n_filt = self.counts[12];

        // + strand A-mods
        if (self.counts[6] + self.counts[8]) > 0
            && observed_mods.contains(&ModCode::a)
        {
            let n_canonical = self.counts[6];
            let n_mod = self.counts[8];
            let filtered_coverage = n_canonical + n_mod;
            let raw_mod_code = ModCode::a.char();
            let n_nocall = self.counts[2];
            let percent_modified =
                n_mod as f32 / (n_mod as f32 + n_canonical as f32);
            let n_diff = self.counts[3]
                .saturating_add(self.counts[4])
                .saturating_add(self.counts[5])
                .saturating_add(self.counts[7])
                .saturating_add(self.counts[9])
                .saturating_add(self.counts[19]);
            counts.push(PileupFeatureCounts {
                strand: Strand::Positive,
                filtered_coverage,
                raw_mod_code,
                fraction_modified: percent_modified,
                n_canonical,
                n_modified: n_mod,
                n_other_modified: 0,
                n_delete: pos_strand_n_delete,
                n_filtered: pos_stand_n_filt,
                n_diff,
                n_nocall,
            });
        }
        // + strand C-mods
        if (self.counts[7] + self.counts[9] + self.counts[10]) > 0 {
            let n_canonical = self.counts[7];
            let n_nocall = self.counts[3];
            let n_diff = self.counts[2]
                .saturating_add(self.counts[4])
                .saturating_add(self.counts[5])
                .saturating_add(self.counts[6])
                .saturating_add(self.counts[8]);

            let n_h = self.counts[9];
            let n_m = self.counts[10];
            let filtered_coverage = n_canonical + n_h + n_m;
            Self::add_pileup_counts(
                pileup_options,
                &mut counts,
                observed_mods,
                Strand::Positive,
                filtered_coverage,
                n_h,
                n_m,
                n_canonical,
                pos_strand_n_delete,
                pos_stand_n_filt,
                n_diff,
                n_nocall,
            );

            // match pileup_options {
            //     PileupNumericOptions::Passthrough
            //     | PileupNumericOptions::Collapse(_) => {
            //         for (mod_code, (n_modified, n_other_modified)) in
            //             [(ModCode::h, (n_h, n_m)), (ModCode::m, (n_m, n_h))]
            //         {
            //             if observed_mods.contains(&mod_code) {
            //                 let percent_modified =
            //                     n_modified as f32 / filtered_coverage as f32;
            //                 counts.push(PileupFeatureCounts {
            //                     strand: Strand::Positive,
            //                     filtered_coverage,
            //                     raw_mod_code: mod_code.char(),
            //                     fraction_modified: percent_modified,
            //                     n_canonical,
            //                     n_modified,
            //                     n_other_modified,
            //                     n_delete: pos_strand_n_delete,
            //                     n_filtered: pos_stand_n_filt,
            //                     n_diff,
            //                     n_nocall,
            //                 })
            //             }
            //         }
            //     }
            //     PileupNumericOptions::Combine => {
            //         let n_modified = n_h + n_m;
            //         let percent_modified =
            //             n_modified as f32 / filtered_coverage as f32;
            //         counts.push(PileupFeatureCounts {
            //             strand: Strand::Positive,
            //             filtered_coverage,
            //             raw_mod_code: ModCode::C.char(),
            //             fraction_modified: percent_modified,
            //             n_canonical,
            //             n_modified,
            //             n_other_modified,
            //             n_delete: pos_strand_n_delete,
            //             n_filtered: pos_stand_n_filt,
            //             n_diff,
            //             n_nocall,
            //         })
            //     }
            // }
        }
        // - strand A-mods
        if (self.counts[17] + self.counts[19]) > 0
            && observed_mods.contains(&ModCode::a)
        {
            let n_canonical = self.counts[17];
            let n_mod = self.counts[19];
            let filtered_coverage = n_canonical + n_mod;
            let raw_mod_code = ModCode::a.char();
            let n_nocall = self.counts[13];
            let percent_modified =
                n_mod as f32 / (n_mod as f32 + n_canonical as f32);
            let n_diff = self.counts[14]
                .saturating_add(self.counts[15])
                .saturating_add(self.counts[16])
                .saturating_add(self.counts[18])
                .saturating_add(self.counts[20])
                .saturating_add(self.counts[21]);
            counts.push(PileupFeatureCounts {
                strand: Strand::Negative,
                filtered_coverage,
                raw_mod_code,
                fraction_modified: percent_modified,
                n_canonical,
                n_modified: n_mod,
                n_other_modified: 0,
                n_delete: neg_strand_n_delete,
                n_filtered: neg_stand_n_filt,
                n_diff,
                n_nocall,
            });
        }
        // - strand C-mods
        if (self.counts[18] + self.counts[20] + self.counts[21]) > 0 {
            let n_canonical = self.counts[18];
            let n_h = self.counts[20];
            let n_m = self.counts[21];
            let filtered_coverage = n_canonical + n_h + n_m;
            let n_nocall = self.counts[14];
            let n_diff = self.counts[13]
                .saturating_add(self.counts[15])
                .saturating_add(self.counts[16])
                .saturating_add(self.counts[17])
                .saturating_add(self.counts[19]);
            Self::add_pileup_counts(
                pileup_options,
                &mut counts,
                observed_mods,
                Strand::Negative,
                filtered_coverage,
                n_h,
                n_m,
                n_canonical,
                neg_strand_n_delete,
                neg_stand_n_filt,
                n_diff,
                n_nocall,
            );

            // for (mod_code, (n_modified, n_other_modified)) in
            //     [(ModCode::h, (n_h, n_m)), (ModCode::m, (n_m, n_h))]
            // {
            //     if check_mod_code(mod_code) {
            //         let percent_modified =
            //             n_modified as f32 / filtered_coverage as f32;
            //         counts.push(PileupFeatureCounts {
            //             strand: Strand::Negative,
            //             filtered_coverage,
            //             raw_mod_code: mod_code.char(),
            //             fraction_modified: percent_modified,
            //             n_canonical,
            //             n_modified,
            //             n_other_modified,
            //             n_delete: neg_strand_n_delete,
            //             n_filtered: neg_stand_n_filt,
            //             n_diff,
            //             n_nocall,
            //         })
            //     }
            // }
        }

        counts
    }
}

struct PileupIter<'a> {
    pileups: bam::pileup::Pileups<'a, bam::IndexedReader>,
    start_pos: u32,
    end_pos: u32,
}

impl<'a> PileupIter<'a> {
    fn new(
        pileups: bam::pileup::Pileups<'a, bam::IndexedReader>,
        start_pos: u32,
        end_pos: u32,
    ) -> Self {
        Self {
            pileups,
            start_pos,
            end_pos,
        }
    }
}

impl<'a> Iterator for PileupIter<'a> {
    type Item = bam::pileup::Pileup;

    fn next(&mut self) -> Option<Self::Item> {
        let mut pileup: Option<Self::Item> = None;
        while let Some(Ok(plp)) = self.pileups.next() {
            let off_end = plp.pos() >= self.end_pos;
            if off_end {
                // we're done
                return None;
            } else if plp.pos() < self.start_pos {
                // advance into region we're looking at
                continue;
            } else {
                pileup = Some(plp);
                break;
            }
        }
        pileup
    }
}

pub struct ModBasePileup {
    pub chrom_name: String,
    position_feature_counts: HashMap<u32, Vec<PileupFeatureCounts>>,
}

impl ModBasePileup {
    pub fn num_results(&self) -> usize {
        self.position_feature_counts.len()
    }

    pub fn iter_counts(
        &self,
    ) -> impl Iterator<Item = (&u32, &Vec<PileupFeatureCounts>)> {
        self.position_feature_counts
            .iter()
            .sorted_by(|(x, _), (y, _)| x.cmp(y))
    }
}

pub enum PileupNumericOptions {
    Passthrough,
    Combine,
    Collapse(CollapseMethod),
}

impl PileupNumericOptions {
    fn get_collapse_method(&self) -> Option<CollapseMethod> {
        match self {
            Self::Collapse(method) => Some(*method),
            _ => None,
        }
    }
}

pub fn process_region<T: AsRef<Path>>(
    bam_fp: T,
    chrom_tid: u32,
    start_pos: u32,
    end_pos: u32,
    threshold: f32,
    pileup_numeric_options: &PileupNumericOptions,
) -> Result<ModBasePileup, String> {
    let mut bam_reader =
        bam::IndexedReader::from_path(bam_fp).map_err(|e| e.to_string())?;
    let chrom_name =
        String::from_utf8_lossy(bam_reader.header().tid2name(chrom_tid))
            .to_string();
    bam_reader
        .fetch(FetchDefinition::Region(
            chrom_tid as i32,
            start_pos as i64,
            end_pos as i64,
        ))
        .map_err(|e| e.to_string())?;

    let mut read_cache =
        ReadCache::new(pileup_numeric_options.get_collapse_method());
    let mut position_feature_counts = HashMap::new();
    let pileup_iter = PileupIter::new(bam_reader.pileup(), start_pos, end_pos);
    for pileup in pileup_iter {
        let mut feature_vector = FeatureVector::new();
        let mut observed_mod_codes = HashSet::new();
        let pos = pileup.pos();

        let alignment_iter = pileup.alignments().filter_map(|alignment| {
            if alignment.is_refskip() {
                None
            } else {
                let record = alignment.record();
                if record_is_secondary(&record) || record.seq_len() == 0 {
                    None
                } else {
                    Some(alignment)
                }
            }
        });
        for alignment in alignment_iter {
            assert!(!alignment.is_refskip());
            let record = alignment.record();
            observed_mod_codes
                .extend(read_cache.get_mod_codes_for_record(&record));
            let strand = if record.is_reverse() {
                Strand::Negative
            } else {
                Strand::Positive
            };

            if alignment.is_del() {
                feature_vector.add_feature(strand, Feature::Delete);
                continue;
            }

            // not delete or skip, add base
            let read_base = alignment.qpos().and_then(|pos| {
                if pos >= record.seq_len() {
                    debug!("Record position is not included in sequence?");
                    None
                } else {
                    DnaBase::parse(record.seq()[pos] as char).ok()
                }
            });

            let read_base = if let Some(base) = read_base {
                if record.is_reverse() {
                    base.complement()
                } else {
                    base
                }
            } else {
                continue;
            };

            let feature = if let Some(mod_call) = read_cache.get_mod_call(
                &record,
                pos,
                read_base.char(),
                threshold,
            ) {
                match mod_call {
                    BaseModCall::Canonical(_) => Feature::ModCall(
                        read_base.canonical_mod_code().unwrap(),
                    ),
                    BaseModCall::Filtered => Feature::Filtered,
                    BaseModCall::Modified(_, raw_code) => Feature::ModCall(
                        ModCode::parse_raw_mod_code(raw_code).unwrap(),
                    ),
                }
            } else {
                Feature::NoCall(read_base)
            };
            feature_vector.add_feature(strand, feature);
        } // alignment loop
        position_feature_counts.insert(
            pos,
            feature_vector.decode(&observed_mod_codes, &pileup_numeric_options),
        );
    } // position loop

    Ok(ModBasePileup {
        chrom_name,
        position_feature_counts,
    })
}

#[cfg(test)]
mod mod_pileup_tests {
    use crate::mod_pileup::{
        DnaBase, Feature, FeatureVector, ModCode, PileupNumericOptions,
    };
    use crate::util::Strand;
    use std::collections::HashSet;

    #[test]
    fn test_feature_vector() {
        let observed_mods = HashSet::from([ModCode::m, ModCode::h]);
        let mut fv = FeatureVector::new();
        fv.add_feature(Strand::Positive, Feature::NoCall(DnaBase::A));
        fv.add_feature(Strand::Positive, Feature::ModCall(ModCode::C));
        fv.add_feature(Strand::Positive, Feature::ModCall(ModCode::m));
        fv.add_feature(Strand::Positive, Feature::ModCall(ModCode::m));
        fv.add_feature(Strand::Positive, Feature::NoCall(DnaBase::C));
        fv.add_feature(Strand::Negative, Feature::NoCall(DnaBase::G));
        fv.add_feature(Strand::Negative, Feature::NoCall(DnaBase::G));
        let counts =
            fv.decode(&observed_mods, &PileupNumericOptions::Passthrough);
        assert_eq!(counts.len(), 2); // h and m, negative strand should not be there
        for pileup_counts in counts {
            assert_eq!(pileup_counts.filtered_coverage, 3);
            assert_eq!(pileup_counts.n_nocall, 1);
            assert_eq!(pileup_counts.n_diff, 1);
            assert_eq!(pileup_counts.strand, Strand::Positive);
        }
        let mut fv = FeatureVector::new();
        fv.add_feature(Strand::Positive, Feature::ModCall(ModCode::C));
        fv.add_feature(Strand::Negative, Feature::ModCall(ModCode::m));
        fv.add_feature(Strand::Negative, Feature::NoCall(DnaBase::G));
        fv.add_feature(Strand::Negative, Feature::NoCall(DnaBase::G));
        let counts =
            fv.decode(&observed_mods, &PileupNumericOptions::Passthrough);
        assert_eq!(counts.len(), 4);
        counts
            .iter()
            .filter(|c| c.strand == Strand::Negative)
            .for_each(|c| assert_eq!(c.n_diff, 2));
    }
}
