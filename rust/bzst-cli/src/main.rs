//! `bzst` — block-compressed zstd: a single, gzip/bgzip-style command-line tool.
//!
//! Compresses by default; `-d` decompresses, `-t` tests integrity, `--list`
//! inspects, and `--cat` concatenates. It operates on files in place — writing
//! `FILE.bzst` and removing `FILE`, gzip-style — unless `-c`/`-o` redirect the
//! output or `-k` keeps the input. With no file it is a stdin→stdout filter.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};

use bzst::{BzstReader, BzstWriter, Index, SeekableReader, Threads};

/// Filename suffix for compressed files.
const SUFFIX: &str = ".bzst";

/// Bytes sampled from the start of the input to classify it in `--mode auto`.
const SNIFF_LEN: usize = 8 << 10;

/// Usage examples appended to `--help`.
const AFTER_HELP: &str = "\
Examples:
  bzst reads.fq                        compress to reads.fq.bzst (removes reads.fq)
  bzst -k -@8 -l9 reads.fq             keep input; 8 threads; level 9
  bzst -d reads.fq.bzst                restore reads.fq
  bzst -c reads.fq > out.bzst          write to stdout, keep input
  bzst --lines-per-record 4 reads.fq   4-line (FASTQ) records; text auto-detected
  bzst -t reads.fq.bzst                test integrity
  bzst --list reads.fq.bzst            header, block sizes, ratio
  bzst --cat a.bzst b.bzst > all.bzst  verbatim concatenate (no recompression)";

/// Block-compressed zstd: parallel, seekable, zstd-compatible compression.
#[derive(Parser)]
#[command(name = "bzst", version, after_help = AFTER_HELP)]
struct Cli {
    /// Decompress.
    #[arg(short, long)]
    decompress: bool,

    /// Test integrity; write no output.
    #[arg(short, long)]
    test: bool,

    /// List contents: header, block sizes, ratio.
    #[arg(long)]
    list: bool,

    /// Concatenate inputs verbatim into one stream (no recompression).
    #[arg(long)]
    cat: bool,

    /// Write to standard output; keep input files.
    #[arg(short = 'c', long)]
    stdout: bool,

    /// Write output to this path (single input only).
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Compression level.
    #[arg(short, long, default_value_t = bzst::DEFAULT_LEVEL, value_name = "INT")]
    level: i32,

    /// Target uncompressed block size, in bytes.
    #[arg(short, long, default_value_t = bzst::DEFAULT_BLOCK_SIZE, value_name = "BYTES")]
    block_size: usize,

    /// Worker threads (0 = all available cores).
    #[arg(short = '@', long, default_value_t = 0, value_name = "INT")]
    threads: usize,

    /// Block-splitting mode (auto detects text vs binary from the input).
    #[arg(short = 'm', long, value_enum, default_value_t = Mode::Auto, value_name = "MODE")]
    mode: Mode,

    /// Keep this many lines together per record in text mode.
    #[arg(long, default_value_t = 1, value_name = "N")]
    lines_per_record: usize,

    /// Overwrite existing output files.
    #[arg(short, long)]
    force: bool,

    /// Keep (do not remove) input files.
    #[arg(short, long)]
    keep: bool,

    /// Input files; with none, read standard input.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,
}

impl Cli {
    /// Resolves the single action, rejecting conflicting mode flags.
    fn action(&self) -> Result<Action> {
        let chosen: Vec<Action> = [
            (self.decompress, Action::Decompress),
            (self.test, Action::Test),
            (self.list, Action::List),
            (self.cat, Action::Cat),
        ]
        .into_iter()
        .filter_map(|(on, action)| on.then_some(action))
        .collect();
        match chosen.as_slice() {
            [] => Ok(Action::Compress),
            [only] => Ok(*only),
            _ => bail!("choose at most one of -d, -t, --list, --cat"),
        }
    }
}

/// The action selected by the mode flags.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Compress,
    Decompress,
    Test,
    List,
    Cat,
}

/// The `--mode` value: how a stream is split into blocks when compressing.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    // Plain `//` comments (not `///`): clap renders per-value help in a verbose
    // multi-line layout, so keep these out of the doc-comment channel.
    // Auto: detect text vs binary from the input, then block accordingly.
    Auto,
    // Bin: binary, fixed-size blocks.
    Bin,
    // Text: end blocks on line boundaries.
    Text,
}

/// A byte transform applied in place or as a filter by [`transform`].
#[derive(Clone, Copy)]
enum Op {
    Compress,
    Decompress,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let action = cli.action()?;
    if cli.lines_per_record < 1 {
        bail!("--lines-per-record must be at least 1");
    }
    if cli.lines_per_record != 1 && action != Action::Compress {
        bail!("--lines-per-record applies only when compressing");
    }
    if cli.lines_per_record != 1 && cli.mode == Mode::Bin {
        bail!("--lines-per-record has no effect with --mode bin (records need text mode)");
    }
    match action {
        Action::Compress => transform(&cli, Op::Compress),
        Action::Decompress => transform(&cli, Op::Decompress),
        Action::Test => test(&cli),
        Action::List => list(&cli),
        Action::Cat => cat(&cli),
    }
}

/// Compresses or decompresses each input (in place, or stdin→stdout with none).
fn transform(cli: &Cli, op: Op) -> Result<()> {
    if cli.files.is_empty() {
        let reader: Box<dyn BufRead> = Box::new(BufReader::new(io::stdin().lock()));
        let writer: Box<dyn Write> = Box::new(BufWriter::new(io::stdout().lock()));
        return transform_stream(cli, op, reader, writer);
    }
    if cli.output.is_some() && cli.files.len() > 1 {
        bail!("-o/--output cannot be combined with multiple input files");
    }
    for path in &cli.files {
        transform_file(cli, op, path).with_context(|| format!("processing {}", path.display()))?;
    }
    Ok(())
}

/// Runs one in-place (or `-c`/`-o`) file transform, honoring the delete/keep
/// rules and cleaning up a partial output on failure.
fn transform_file(cli: &Cli, op: Op, path: &Path) -> Result<()> {
    let reader: Box<dyn BufRead> = Box::new(BufReader::new(
        File::open(path).with_context(|| format!("opening {}", path.display()))?,
    ));
    if cli.stdout {
        let writer: Box<dyn Write> = Box::new(BufWriter::new(io::stdout().lock()));
        return transform_stream(cli, op, reader, writer);
    }
    let out = match &cli.output {
        Some(o) => o.clone(),
        None => derived_output(op, path)?,
    };
    if same_file(path, &out) {
        bail!("input and output are the same file: {}", path.display());
    }
    if out.exists() && !cli.force {
        bail!("{} already exists; use -f to overwrite", out.display());
    }
    let writer: Box<dyn Write> = Box::new(BufWriter::new(
        File::create(&out).with_context(|| format!("creating {}", out.display()))?,
    ));
    if let Err(e) = transform_stream(cli, op, reader, writer) {
        let _ = fs::remove_file(&out); // don't leave a partial/corrupt output behind
        return Err(e);
    }
    // gzip-style: remove the input only in the default in-place case.
    if !cli.keep && cli.output.is_none() {
        fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

fn transform_stream(
    cli: &Cli,
    op: Op,
    reader: Box<dyn BufRead>,
    writer: Box<dyn Write>,
) -> Result<()> {
    match op {
        Op::Compress => compress(cli, reader, writer),
        Op::Decompress => decompress(cli, reader, writer),
    }
}

fn compress(cli: &Cli, reader: Box<dyn BufRead>, writer: Box<dyn Write>) -> Result<()> {
    let (text, mut reader) = resolve_text_mode(cli, reader)?;
    // In text mode the record splitter drives every block boundary via
    // end_block(); the writer must not also cut at block_size (that would split
    // whichever record straddles the offset), so build it unbounded and let the
    // splitter alone decide where blocks end.
    let writer_block_size = if text { usize::MAX } else { cli.block_size };
    let mut w = BzstWriter::builder(writer)
        .level(cli.level)
        .block_size(writer_block_size)
        .threads(threads_of(cli.threads))
        .build()
        .context("creating bzst writer")?;
    if text {
        write_text(&mut *reader, &mut w, cli.block_size, cli.lines_per_record)?;
    } else {
        io::copy(&mut *reader, &mut w).context("compressing")?;
    }
    w.finish().context("finishing bzst stream")?;
    Ok(())
}

/// Decides whether to block as text, sniffing the input for `--mode auto`.
/// Returns the decision and a reader that still yields the full input: any bytes
/// sampled for sniffing are buffered and chained back ahead of the rest, so this
/// works on pipes/FIFOs as well as regular files (no seeking required).
fn resolve_text_mode(cli: &Cli, reader: Box<dyn BufRead>) -> Result<(bool, Box<dyn BufRead>)> {
    match cli.mode {
        Mode::Bin => Ok((false, reader)),
        Mode::Text => Ok((true, reader)),
        Mode::Auto => sniff_text(reader),
    }
}

fn sniff_text(mut reader: Box<dyn BufRead>) -> Result<(bool, Box<dyn BufRead>)> {
    let mut prefix = vec![0u8; SNIFF_LEN];
    let mut filled = 0;
    while filled < prefix.len() {
        match reader.read(&mut prefix[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    prefix.truncate(filled);
    let is_text = looks_like_text(&prefix);
    // Re-supply the sniffed bytes ahead of the rest of the stream.
    let reader: Box<dyn BufRead> = Box::new(Cursor::new(prefix).chain(reader));
    Ok((is_text, reader))
}

/// Conservative text detection over a sampled prefix: every byte must be
/// printable ASCII or common text whitespace (tab, CR, LF), and there must be
/// at least one line boundary to break on. Any control character, DEL, or
/// non-ASCII byte (0x80 or above, including UTF-8 multibyte text) makes the
/// input binary. The two failure modes aren't symmetric, so this errs toward
/// binary: binary misread as text lets stray 0x0A bytes drive block boundaries
/// and ignore the size target (risking pathological block sizes), whereas text
/// misread as binary only splits records across blocks while still honoring the
/// target size. A wrong guess changes where blocks fall, never correctness.
fn looks_like_text(prefix: &[u8]) -> bool {
    let mut has_newline = false;
    for &b in prefix {
        match b {
            b'\n' => has_newline = true,
            b'\t' | b'\r' => {} // common text whitespace
            0x20..=0x7E => {}   // printable ASCII
            _ => return false,  // control char, DEL, or non-ASCII byte (>= 0x80)
        }
    }
    has_newline
}

fn decompress(cli: &Cli, reader: Box<dyn BufRead>, mut writer: Box<dyn Write>) -> Result<()> {
    let mut r = BzstReader::builder(reader)
        .threads(threads_of(cli.threads))
        .build()
        .context("opening bzst stream")?;
    io::copy(&mut r, &mut writer).context("decompressing")?;
    writer.flush()?;
    Ok(())
}

/// Text-mode splitting: cut a block at a line boundary once at least
/// `block_size` bytes have accumulated, never splitting a `lines_per_record`
/// group of lines across a block.
///
/// Single-line records (`lines_per_record == 1` — SAM/VCF/BED, and the default)
/// take a bulk path that scans for a newline only when a block is large enough
/// to cut, avoiding a per-line copy into an intermediate buffer. Multi-line
/// records (e.g. 4-line FASTQ) keep the straightforward per-record grouping,
/// which must track record boundaries line by line anyway.
fn write_text<W: Write>(
    r: &mut dyn BufRead,
    w: &mut BzstWriter<W>,
    block_size: usize,
    lines_per_record: usize,
) -> Result<()> {
    if lines_per_record == 1 {
        write_lines(r, w, block_size)
    } else {
        write_records(r, w, block_size, lines_per_record)
    }
}

/// Single-line-record text splitting (the common case). Stages input in bulk and
/// cuts a block at the first newline at or after the point where the staged
/// bytes reach `block_size`, so blocks stay line-aligned without copying each
/// line through an intermediate buffer.
fn write_lines<W: Write>(
    r: &mut dyn BufRead,
    w: &mut BzstWriter<W>,
    block_size: usize,
) -> Result<()> {
    let mut since = 0usize; // bytes staged toward the current (uncut) block
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            break; // EOF; finish() emits any trailing partial line as the last block
        }
        let n = chunk.len();
        if since + n < block_size {
            // Still short of the target: stage the whole chunk and read on.
            w.write_all(chunk)?;
            since += n;
            r.consume(n);
            continue;
        }
        // The block reaches its target within this chunk. Cut at the first newline
        // at or after the crossing point so the block ends on a line boundary.
        let cross = block_size.saturating_sub(since);
        match chunk[cross..].iter().position(|&b| b == b'\n') {
            Some(rel) => {
                let cut = cross + rel + 1; // include the newline in this block
                w.write_all(&chunk[..cut])?;
                w.end_block()?;
                since = 0;
                r.consume(cut);
            }
            None => {
                // No newline past the crossing point; stage the whole chunk and cut
                // in a later one (this block runs a little past its target).
                w.write_all(chunk)?;
                since += n;
                r.consume(n);
            }
        }
    }
    Ok(())
}

/// Multi-line-record text splitting: reads `lines_per_record` lines at a time and
/// never splits a record across a block, cutting once a block reaches `block_size`.
fn write_records<W: Write>(
    r: &mut dyn BufRead,
    w: &mut BzstWriter<W>,
    block_size: usize,
    lines_per_record: usize,
) -> Result<()> {
    let mut record = Vec::new();
    let mut since = 0usize;
    loop {
        record.clear();
        let mut lines = 0;
        while lines < lines_per_record {
            if r.read_until(b'\n', &mut record)? == 0 {
                break;
            }
            lines += 1;
        }
        if record.is_empty() {
            break;
        }
        w.write_all(&record)?;
        since += record.len();
        if since >= block_size {
            w.end_block()?;
            since = 0;
        }
    }
    Ok(())
}

/// The default output path for `input`: append `.bzst` when compressing, strip
/// it when decompressing.
fn derived_output(op: Op, input: &Path) -> Result<PathBuf> {
    match op {
        Op::Compress => {
            let mut name = input.as_os_str().to_owned();
            name.push(SUFFIX);
            Ok(PathBuf::from(name))
        }
        Op::Decompress => {
            let name =
                input.to_str().ok_or_else(|| anyhow!("{}: non-UTF-8 filename", input.display()))?;
            match name.strip_suffix(SUFFIX) {
                Some(stripped) => Ok(PathBuf::from(stripped)),
                None => bail!(
                    "{}: unknown suffix, expected {SUFFIX} (use -o to name output)",
                    input.display()
                ),
            }
        }
    }
}

/// Tests the integrity of each input (or stdin), reporting per-file OK/FAILED.
fn test(cli: &Cli) -> Result<()> {
    if cli.files.is_empty() {
        let reader: Box<dyn BufRead> = Box::new(BufReader::new(io::stdin().lock()));
        let mut r = BzstReader::new(reader).context("testing stdin")?;
        io::copy(&mut r, &mut io::sink()).context("testing stdin")?;
        println!("stdin: OK");
        return Ok(());
    }
    let mut failures = 0usize;
    for path in &cli.files {
        match test_file(cli, path) {
            Ok(()) => println!("{}: OK", path.display()),
            Err(e) => {
                eprintln!("{}: FAILED: {e:#}", path.display());
                failures += 1;
            }
        }
    }
    if failures > 0 {
        bail!("{failures} file(s) failed the integrity check");
    }
    Ok(())
}

fn test_file(cli: &Cli, path: &Path) -> Result<()> {
    // Streaming decode validates the header, every block-header checksum, and
    // every data frame (its zstd content checksum and exact decoded size).
    let reader = BufReader::new(File::open(path)?);
    let mut r = BzstReader::builder(reader).threads(threads_of(cli.threads)).build()?;
    io::copy(&mut r, &mut io::sink())?;
    // Streaming decode never consults the index, so validate it against a rebuild
    // from the block-header frames (this also checks the EOF trailer).
    let stored = Index::read_from(&mut File::open(path)?)?;
    let rebuilt = Index::rebuild(File::open(path)?)?;
    if stored != rebuilt {
        bail!("index does not match the blocks (stale or corrupt)");
    }
    Ok(())
}

/// Prints header, block-size statistics, and totals for each input.
fn list(cli: &Cli) -> Result<()> {
    if cli.files.is_empty() {
        bail!("--list requires a file (it needs a seekable input)");
    }
    for (i, path) in cli.files.iter().enumerate() {
        if i > 0 {
            println!();
        }
        list_file(path).with_context(|| format!("reading {}", path.display()))?;
    }
    Ok(())
}

fn list_file(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    let compressed = file.metadata()?.len();
    let sr = SeekableReader::new(file).context("reading bzst header/index")?;
    let header = sr.header();
    let index = sr.index();
    let uncompressed = sr.total_uncompressed();
    let blocks = index.len();
    println!("file                {}", path.display());
    println!("format version      {}", header.format_version);
    println!("format signature    {:?}", String::from_utf8_lossy(&header.format_signature));
    println!(
        "profiles            {}",
        if header.profiles.is_baseline() { "baseline" } else { "advanced" }
    );
    println!("blocks              {blocks}");
    if blocks > 0 {
        let (mut min, mut max) = (u64::MAX, 0u64);
        for i in 0..blocks {
            let size = index.uncompressed_block_size(i).expect("block index in range");
            min = min.min(size);
            max = max.max(size);
        }
        let avg = (uncompressed as f64 / blocks as f64).round() as u64;
        println!("block sizes         min {min}, avg {avg}, max {max} (uncompressed bytes)");
    }
    println!("uncompressed bytes  {uncompressed}");
    println!("compressed bytes    {compressed}");
    if compressed > 0 {
        println!("ratio               {:.3}", uncompressed as f64 / compressed as f64);
    }
    Ok(())
}

/// Concatenates all inputs verbatim into one bzst stream (stdout or `-o`).
fn cat(cli: &Cli) -> Result<()> {
    if cli.files.is_empty() {
        bail!("--cat requires input files");
    }
    if let Some(o) = &cli.output {
        if cli.files.iter().any(|input| same_file(input, o)) {
            bail!("output {} is also an input file", o.display());
        }
    }
    let mut readers = Vec::with_capacity(cli.files.len());
    for path in &cli.files {
        readers.push(BufReader::new(
            File::open(path).with_context(|| format!("opening {}", path.display()))?,
        ));
    }
    let writer: Box<dyn Write> = match &cli.output {
        Some(o) => {
            if o.exists() && !cli.force {
                bail!("{} already exists; use -f to overwrite", o.display());
            }
            Box::new(BufWriter::new(
                File::create(o).with_context(|| format!("creating {}", o.display()))?,
            ))
        }
        None => Box::new(BufWriter::new(io::stdout().lock())),
    };
    bzst::concat(readers, writer).context("concatenating")?;
    Ok(())
}

fn threads_of(n: usize) -> Threads {
    if n == 1 {
        Threads::Serial
    } else {
        Threads::Owned(n)
    }
}

/// True if `a` and `b` resolve to the same existing file, so an operation would
/// clobber its own input. Paths that don't both exist can't be the same file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_text;

    #[test]
    fn ascii_text_is_text() {
        assert!(looks_like_text(b"the quick brown fox\njumps over the lazy dog\n"));
    }

    #[test]
    fn tab_and_crlf_are_text() {
        // Tab-delimited with Windows line endings (e.g. VCF/BED/SAM).
        assert!(looks_like_text(b"col1\tcol2\tcol3\r\nv1\tv2\tv3\r\n"));
    }

    #[test]
    fn high_bytes_are_binary() {
        // Non-ASCII (UTF-8 multibyte) is treated as binary, by design.
        assert!(!looks_like_text("café au lait\nnaïve façade\n".as_bytes()));
    }

    #[test]
    fn nul_byte_is_binary() {
        assert!(!looks_like_text(b"looks texty\nbut has a\0nul byte\n"));
    }

    #[test]
    fn a_single_control_char_is_binary() {
        // One stray control byte (here ESC) is enough to fall back to binary.
        assert!(!looks_like_text(b"mostly text\nwith one \x1b escape\n"));
    }

    #[test]
    fn text_without_a_newline_is_not_text() {
        assert!(!looks_like_text(b"one long line with no terminator at all"));
    }

    #[test]
    fn empty_is_not_text() {
        assert!(!looks_like_text(b""));
    }
}
