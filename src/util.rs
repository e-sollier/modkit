use anyhow::anyhow;

use std::string::FromUtf8Error;

use anyhow::Result as AnyhowResult;
use derive_new::new;
use indicatif::{ProgressBar, ProgressStyle};
use log::debug;
use rust_htslib::bam::{
    self, ext::BamRecordExtensions, header::HeaderRecord, record::Aux,
    HeaderView,
};

use crate::errs::{InputError, RunError};

pub(crate) fn get_spinner() -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template(
            "{spinner:.blue} [{elapsed_precise}] {pos} {msg}",
        )
        .unwrap()
        .tick_strings(&[
            "▹▹▹▹▹",
            "▸▹▹▹▹",
            "▹▸▹▹▹",
            "▹▹▸▹▹",
            "▹▹▹▸▹",
            "▹▹▹▹▸",
            "▪▪▪▪▪",
        ]),
    );
    spinner
}

fn get_master_progress_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "[{elapsed_precise}] {bar:40.green/yellow} {pos:>7}/{len:7} {msg}",
    )
    .unwrap()
    .progress_chars("##-")
}

fn get_subroutine_progress_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "[{elapsed_precise}] {bar:40.blue/cyan} {pos:>7}/{len:7} {msg}",
    )
    .unwrap()
    .progress_chars("##-")
}

pub(crate) fn get_master_progress_bar(n: usize) -> ProgressBar {
    ProgressBar::new(n as u64).with_style(get_master_progress_bar_style())
}

pub(crate) fn get_subroutine_progress_bar(n: usize) -> ProgressBar {
    ProgressBar::new(n as u64).with_style(get_subroutine_progress_bar_style())
}

pub(crate) fn get_aligned_pairs_forward(
    record: &bam::Record,
) -> impl Iterator<Item = AnyhowResult<(usize, u64)>> + '_ {
    let read_length = record.seq_len();
    record.aligned_pairs().map(move |pair| {
        let q_pos = pair[0] as usize;
        let q_pos = if record.is_reverse() {
            read_length
                .checked_sub(q_pos)
                .and_then(|x| x.checked_sub(1))
        } else {
            Some(q_pos)
        };
        if q_pos.is_none() || pair[1] < 0 {
            let read_id = get_query_name_string(&record)
                .unwrap_or("failed-to-parse-utf8".to_owned());
            debug!("record {read_id} has invalid aligned pair {:?}", pair);
            return Err(anyhow!("pair {:?} is invalid", pair));
        }

        let r_pos = pair[1];
        assert!(r_pos >= 0);
        let r_pos = r_pos as u64;
        Ok((q_pos.unwrap(), r_pos))
    })
}

pub(crate) fn get_query_name_string(
    record: &bam::Record,
) -> Result<String, FromUtf8Error> {
    String::from_utf8(record.qname().to_vec())
}

#[inline]
pub(crate) fn get_forward_sequence(
    record: &bam::Record,
) -> Result<String, RunError> {
    let raw_seq = if record.is_reverse() {
        bio::alphabets::dna::revcomp(record.seq().as_bytes())
    } else {
        record.seq().as_bytes()
    };
    let seq = String::from_utf8(raw_seq).map_err(|e| {
        RunError::new_input_error(format!(
            "failed to convert sequence to string, {}",
            e
        ))
    })?;
    if seq.len() == 0 {
        return Err(RunError::new_failed("seq is empty"));
    }
    Ok(seq)
}

pub(crate) fn get_tag<T>(
    record: &bam::Record,
    tag_keys: &[&'static str; 2],
    parser: &dyn Fn(&Aux, &str) -> Result<T, RunError>,
) -> Option<Result<(T, &'static str), RunError>> {
    let tag_new = record.aux(tag_keys[0].as_bytes());
    let tag_old = record.aux(tag_keys[1].as_bytes());

    let tag = match (tag_new, tag_old) {
        (Ok(aux), _) => Some((aux, tag_keys[0])),
        (Err(_), Ok(aux)) => Some((aux, tag_keys[1])),
        _ => None,
    };

    tag.map(|(aux, t)| {
        let parsed = parser(&aux, t);
        parsed.map(|res| (res, t))
    })
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash, Default)]
pub enum Strand {
    #[default]
    Positive,
    Negative,
}

impl Strand {
    pub fn parse_char(x: char) -> Result<Self, InputError> {
        match x {
            '+' => Ok(Self::Positive),
            '-' => Ok(Self::Negative),
            _ => Err(format!("failed to parse strand {}", x).into()),
        }
    }
    pub fn to_char(&self) -> char {
        match self {
            Strand::Positive => '+',
            Strand::Negative => '-',
        }
    }

    pub fn opposite(&self) -> Self {
        match self {
            Strand::Positive => Strand::Negative,
            Strand::Negative => Strand::Positive,
        }
    }
}

pub fn record_is_secondary(record: &bam::Record) -> bool {
    record.is_supplementary() || record.is_secondary() || record.is_duplicate()
}

pub(crate) fn get_targets(
    header: &HeaderView,
    region: Option<&Region>,
) -> Vec<ReferenceRecord> {
    (0..header.target_count())
        .filter_map(|tid| {
            let chrom_name = String::from_utf8(header.tid2name(tid).to_vec())
                .unwrap_or("???".to_owned());
            if let Some(region) = &region {
                if chrom_name == region.name {
                    Some(ReferenceRecord::new(
                        tid,
                        region.start,
                        region.length(),
                        chrom_name,
                    ))
                } else {
                    None
                }
            } else {
                match header.target_len(tid) {
                    Some(size) => {
                        Some(ReferenceRecord::new(tid, 0, size as u32, chrom_name))
                    }
                    None => {
                        debug!("> no size information for {chrom_name} (tid: {tid})");
                        None
                    }
                }
            }
        })
        .collect::<Vec<ReferenceRecord>>()
}

#[derive(Debug, new)]
pub struct ReferenceRecord {
    pub tid: u32,
    pub start: u32,
    pub length: u32,
    pub name: String,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Region {
    pub name: String,
    pub start: u32,
    pub end: u32,
}

impl Region {
    pub fn length(&self) -> u32 {
        self.end - self.start
    }

    fn parse_raw_with_start_and_end(raw: &str) -> Result<Self, InputError> {
        let mut splitted = raw.split(':');
        let chrom_name = splitted
            .nth(0)
            .ok_or(InputError::new(&format!("failed to parse region {raw}")))?;
        let start_end = splitted.collect::<Vec<&str>>();
        if start_end.len() != 1 {
            return Err(InputError::new(&format!(
                "failed to parse region {raw}"
            )));
        } else {
            let start_end = start_end[0];
            let splitted = start_end
                .split('-')
                .map(|x| {
                    x.parse::<u32>()
                        .map_err(|e| InputError::new(&e.to_string()))
                })
                .collect::<Result<Vec<u32>, _>>()?;
            if splitted.len() != 2 {
                return Err(InputError::new(&format!(
                    "failed to parse region {raw}"
                )));
            } else {
                let start = splitted[0];
                let end = splitted[1];
                if end <= start {
                    return Err(InputError::new(&format!(
                        "failed to parse region {raw}, end must be after start"
                    )));
                }
                Ok(Self {
                    name: chrom_name.to_owned(),
                    start,
                    end,
                })
            }
        }
    }

    pub fn parse_str(
        raw: &str,
        header: &HeaderView,
    ) -> Result<Self, InputError> {
        if raw.contains(':') {
            Self::parse_raw_with_start_and_end(raw)
        } else {
            let target_id = (0..header.target_count()).find_map(|tid| {
                String::from_utf8(header.tid2name(tid).to_vec())
                    .ok()
                    .and_then(
                        |contig| if &contig == raw { Some(tid) } else { None },
                    )
            });
            let target_length =
                target_id.and_then(|tid| header.target_len(tid));
            if let Some(len) = target_length {
                Ok(Self {
                    name: raw.to_owned(),
                    start: 0,
                    end: len as u32,
                })
            } else {
                Err(InputError::new(&format!(
                    "failed to find matching reference sequence for {raw} in BAM header"
                )))
            }
        }
    }

    pub fn get_fetch_definition(
        &self,
        header: &HeaderView,
    ) -> AnyhowResult<bam::FetchDefinition> {
        let tid = (0..header.target_count())
            .find_map(|tid| {
                String::from_utf8(header.tid2name(tid).to_vec())
                    .ok()
                    .and_then(|chrom| {
                        if &chrom == &self.name {
                            Some(tid)
                        } else {
                            None
                        }
                    })
            })
            .ok_or(anyhow!(
                "failed to find target ID for chrom {}",
                self.name.as_str()
            ))?;
        let tid = tid as i32;
        Ok(bam::FetchDefinition::Region(
            tid,
            self.start as i64,
            self.end as i64,
        ))
    }

    pub(crate) fn to_string(&self) -> String {
        format!("{}:{}-{}", self.name, self.start, self.end)
    }
}

pub fn add_modkit_pg_records(header: &mut bam::Header) {
    let header_map = header.to_hashmap();
    let (id, pp) = if let Some(pg_tags) = header_map.get("PG") {
        let modkit_invocations = pg_tags.iter().filter_map(|tags| {
            tags.get("ID").and_then(|v| {
                if v.contains("modkit") {
                    let last_run = v.split('.').nth(1).unwrap_or("0");
                    last_run.parse::<usize>().ok()
                } else {
                    None
                }
            })
        });
        if let Some(latest_run_number) = modkit_invocations.max() {
            let pp = if latest_run_number > 0 {
                Some(format!("modkit.{}", latest_run_number))
            } else {
                Some(format!("modkit"))
            };
            (format!("modkit.{}", latest_run_number + 1), pp)
        } else {
            (format!("modkit"), None)
        }
    } else {
        (format!("modkit"), None)
    };

    let command_line = std::env::args().collect::<Vec<String>>();
    let command_line = command_line.join(" ");
    let version = env!("CARGO_PKG_VERSION");
    let mut modkit_header_record = HeaderRecord::new("PG".as_bytes());
    modkit_header_record.push_tag("ID".as_bytes(), &id);
    modkit_header_record.push_tag("PN".as_bytes(), &"modkit".to_owned());
    modkit_header_record.push_tag("VN".as_bytes(), &version.to_owned());
    if let Some(pp) = pp {
        modkit_header_record.push_tag("PP".as_bytes(), &pp);
    }
    modkit_header_record.push_tag("CL".as_bytes(), &command_line);

    header.push_record(&modkit_header_record);
}
