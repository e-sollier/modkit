![Oxford Nanopore Technologies logo](https://github.com/epi2me-labs/modbam2bed/raw/master/images/ONT_logo_590x106.png)

# Modkit

A bioinformatics tool for working with modified bases from Oxford Nanopore. Specifically for converting modBAM
to bedMethyl files using best practices, but also manipulating modBAM files and generating summary statistics.

## Installation

Downloadable pre-compiled binaries are provided for macOS and linux under the "releases" tab. To build
`modkit` locally the recommended way is to use (cargo)[https://www.rust-lang.org/learn/get-started].

```bash
git clone https://github.com/nanoporetech/modkit.git
cd modkit
cargo install --path .
# or cargo install --git https://github.com/nanoporetech/modkit.git
```

## Creating a bedMethyl pileup from a modBam

The most typical use case is to take a BAM with modified bases (as MM/ML or Mm/Ml tags) and sum the base
modification calls from every read over each reference genomic position (a pileup). 

```bash
modkit pileup path/to/reads.bam output/path/pileup.bed 
```

No reference sequence is required. A single file (description below) with pileup calls will be created.
Modification filtering will be performed for you (for details see `filtering.md`).

Some typical options:

1. Only emit counts from reference CpG dinucleotides. This option, however, requires a reference sequence in
   order to locate the CpGs in the reference.

```bash
modkit pileup path/to/reads.bam output/path/pileup.bed --cpg --ref path/to/reference.fasta
```

2. Use `traditional` preset for strand-aggregated 5mCpG-only output.

```bash
modkit pileup path/to/reads.bam output/path/pileup.bed --ref path/to/reference.fasta --preset traditional
```

The `--preset traditional` option will restrict output to only locations where there is a CG dinucleotide in
the reference _as well as_ ignore any modification calls that are not 5mC. For example, if you have 5hmC calls
in your data, they will be ignored by applying the default redistribute method (see collapse.md).
Strand-aggregation is also performed *(summing counts into the positive strand). This option is short hand for
`modkit pileup --cpg --ref <reference.fasta> --collapse h --combine-strands`.  For more information on the
individual options see the advanced_usage.md document.

## bedMethyl output description

Below is a description of the bedMethyl columns generated by `modkit pileup`. A brief description of the
bedMethyl specification can be found on [Encode](https://www.encodeproject.org/data-standards/wgbs/).

### Definitions:

**N_mod**: Number of filtered calls that classified a residue as with a specific base modification.  For
example, if the base modification is `h` (5hmC) then this number is the number of filtered reads with a 5hmC
call aligned to this reference position.

**N_canonical**: Number of filtered calls that classified a residue as canonical as opposed to modified. The
exact base must be inferred by the modification code. For example, if the modification code is `m` (5mC) then
the canonical base is cytosine. If the modification code is `a`, the canonical base is adenosine.

**N_other_mod**: Number of filtered calls that classified a residue as modified where the canonical base is the
same, but the actual modification is different. For example, for a given cytosine there may be 3 reads with
`h` calls, 1 with a canonical call, and 2 with `m` calls. In the row for `h` N_other_mod would be 2 and in the
`m` row N_other_mod would be 3.

**filtered_coverage**: N_mod + N_other_mod + N_canonical, also used as the `score` in the bedMethyl

**N_diff**: Number of reads with a base other than the canonical base for this modification. For example, in a row
for `h` the canonical base is cytosine, if there are 2 reads with C->A substitutions, N_diff will be 2.

**N_delete**: Number of reads with a delete at this reference position

**N_filtered**: Number of calls where the probability of the call was below the threshold. The threshold can be
set on the command line or computed from the data (usually filtering out the lowest 10th percentile of calls).

**N_nocall**: Number of reads aligned to this reference position, with the correct canonical base, but without a base
modification call. This can happen, for example, if the model requires a CpG dinucleotide and the read has a
CG->CH substitution.

### bedMethyl column descriptions

| column | name              | description                                                                                                 | type  |
|--------|-------------------|-------------------------------------------------------------------------------------------------------------|-------|
| 1      | chrom             | name of reference sequence from BAM header                                                                  | str   |
| 2      | start_pos         | 0-based start position                                                                                      | int   |
| 3      | end_pos           | 0-based exclusive end position                                                                              | int   |
| 4      | raw_mod_code      | single letter code of modified base                                                                         | str   |
| 5      | score             | filtered_coverage                                                                                           | int   |
| 6      | strand            | '+' for positive strand '-' for negative strand, '.' when strands are combined                              | str   |
| 7      | start_pos         | included for compatibility                                                                                  | int   |
| 8      | end_pos           | included for compatibility                                                                                  | int   |
| 9      | color             | included for compatibility, always 255,0,0                                                                  | str   |
| 10     | filtered_coverage | see definitions                                                                                             | int   |
| 11     | percent_modified  | N_mod / filtered_coverage                                                                                   | float |
| 12     | N_mod             | Number of filtered calls for raw_mod_code.                                                                  | int   |
| 13     | N_canonical       | Number of filtered calls for a canonical residue.                                                           | int   |
| 14     | N_other_mod       | Number of filtered calls for a modification other than raw_mod_code.                                        | int   |
| 15     | N_delete          | Number of reads with a deletion at this reference position.                                                 | int   |
| 16     | N_filtered        | Number of calls that were filtered out.                                                                     | int   |
| 17     | N_diff            | Number of reads with a base other than the reference sequence canonical base corresponding to raw_mod_code. | int   |
| 18     | N_nocall          | Number of reads with no base modification information at this reference position.                           | int   |



## Advanced usage examples

1. Combine multiple base modification calls into one, for example if your data has 5hmC and 5mC
   this will combine the counts into a `C` (any mod) count.

```bash
modkit pileup path/to/reads.bam output/path/pileup.bed --combine-mods
```

2. CpG motifs are reverse complement equivalent. The following example combines the calls from the positive
   stand C with the negative strand C (reference G). This operation _requires_ that you use the `--cpg` flag
   and specify a reference sequence. The strand field will be marked as '.' indicating that the strand
   information has been lost.

```bash
modkit pileup path/to/reads.bam output/path/pileup.bed --cpg --ref path/to/reference.fasta \
    --combine-strands  
```

3. Produce a bedGraph for each modification in the BAM file file. Counts for the positive and negative strands
   will be put in separate files. Can also be combined with `--cpg` and `--combine-strands` options. The
   `--prefix [str]` option allows specification of a prefix to the output file names.

```bash
modkit pileup path/to/reads.bam output/directory/path --bedgraph <--prefix string>
```


## Terms and Licence

TODO

**Licence and Copyright**

© 2023 Oxford Nanopore Technologies Ltd.  Modkit is distributed under the terms of the Oxford Nanopore
Technologies' Public Licence.
