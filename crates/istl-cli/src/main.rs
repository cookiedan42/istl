//! Command-line interface to the `istl` crate.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use istl_core::{Bbox, DEFAULT_GROUP_SIZE, QueryStats, Reader, convert};

#[derive(Parser)]
#[command(name = "istl", version, about = "Indexed STL converter and reader")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum OutputFormat {
    /// Plain binary STL (80-byte zero header + u32 count + triangles).
    Stl,
    /// Indexed binary STL — matched row groups are streamed verbatim and
    /// a new footer is written. The result is a valid `.istl` and can be
    /// queried again.
    Istl,
}

#[derive(Subcommand)]
enum Cmd {
    /// Convert a plain binary STL to a Hilbert-sorted indexed .istl file.
    ///
    /// `--group-size` controls triangles per row group. Default (32,768) targets
    /// object-store access (S3/GCS/Azure) — ~1.6 MB per group, footer fits a
    /// ~64 KB tail GET. Lower (8,192) is better for local NVMe / tight AOIs;
    /// higher (131,072+) for HDD/NFS or generic HTTP servers.
    Convert {
        input: PathBuf,
        output: PathBuf,
        #[arg(long, default_value_t = DEFAULT_GROUP_SIZE)]
        group_size: usize,
    },
    /// Query an .istl file with a 3D bbox and write the result.
    ///
    /// `input` and `output` are each a local path or a URL. Supported URL
    /// schemes (require the `remote` feature, on by default): `s3://`,
    /// `gs://`, `az://`, `http(s)://`, `file://`, `memory://`.
    /// Credentials/region come from the standard provider chains (env,
    /// profile, IMDS, Lambda role, etc.).
    ///
    /// `--bbox xmin,ymin,zmin,xmax,ymax,zmax` — use `-inf`/`inf` to disable an axis.
    Query {
        input: String,
        output: String,
        #[arg(long, value_parser = parse_bbox)]
        bbox: Bbox,
        /// Output format. Default `stl` emits a vanilla binary STL; `istl`
        /// emits an indexed `.istl` re-using the matched row groups verbatim.
        #[arg(long, value_enum, default_value_t = OutputFormat::Stl)]
        format: OutputFormat,
    },
    /// Print summary info about an .istl file's footer.
    Info {
        input: PathBuf,
        #[arg(long, default_value_t = 3)]
        sample: usize,
    },
}

/// Parses "xmin,ymin,zmin,xmax,ymax,zmax" into a `Bbox`.
fn parse_bbox(s: &str) -> Result<Bbox, String> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 6 {
        return Err(format!(
            "expected 6 comma-separated floats (xmin,ymin,zmin,xmax,ymax,zmax), got {}",
            parts.len()
        ));
    }
    let v: Vec<f32> = parts
        .iter()
        .map(|p| p.parse::<f32>().map_err(|e| format!("{p:?}: {e}")))
        .collect::<Result<_, _>>()?;
    let (xmin, ymin, zmin, xmax, ymax, zmax) = (v[0], v[1], v[2], v[3], v[4], v[5]);
    if !(xmin <= xmax && ymin <= ymax && zmin <= zmax) {
        return Err("bbox min > max on at least one axis".into());
    }
    Ok(Bbox {
        xmin,
        xmax,
        ymin,
        ymax,
        zmin,
        zmax,
    })
}

/// Classify an `input`/`output` argument.
///
/// - A `file://` URL is unwrapped to its local path (cross-platform via
///   `url::Url::to_file_path()` when the `remote` feature is on; on a build
///   without `remote`, `file://` falls back to naive prefix stripping).
/// - Anything else that matches `<scheme>://` (RFC 3986 scheme syntax) is a
///   remote URL.
/// - Otherwise the argument is treated as a local path.
enum Endpoint {
    Local(PathBuf),
    Remote(String),
}

fn classify(s: &str) -> Endpoint {
    if let Some(rest) = strip_file_scheme(s) {
        return Endpoint::Local(rest);
    }
    if looks_like_url(s) {
        return Endpoint::Remote(s.to_string());
    }
    Endpoint::Local(PathBuf::from(s))
}

/// True if `s` starts with `<scheme>://` per RFC 3986 scheme syntax
/// (`ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`).
fn looks_like_url(s: &str) -> bool {
    let bytes = s.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let mut i = 1;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b':' {
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == b'+' || c == b'-' || c == b'.') {
            return false;
        }
        i += 1;
    }
    // need "://" immediately after the scheme
    s.as_bytes().get(i..i + 3) == Some(b"://")
}

#[cfg(feature = "remote")]
fn strip_file_scheme(s: &str) -> Option<PathBuf> {
    if !s.starts_with("file://") {
        return None;
    }
    let url = url::Url::parse(s).ok()?;
    url.to_file_path().ok()
}

#[cfg(not(feature = "remote"))]
fn strip_file_scheme(s: &str) -> Option<PathBuf> {
    s.strip_prefix("file://").map(PathBuf::from)
}

/// Open a reader over a local path or a remote URL.
enum AnyReader {
    Local(Reader<istl_core::FileSource>),
    #[cfg(feature = "remote")]
    Remote(Reader<istl_objectstore::ObjectStoreSource>),
}

impl AnyReader {
    fn open(endpoint: Endpoint) -> Result<Self> {
        match endpoint {
            Endpoint::Local(p) => Ok(AnyReader::Local(
                Reader::open(&p).with_context(|| format!("opening {:?}", p))?,
            )),
            #[cfg(feature = "remote")]
            Endpoint::Remote(url) => {
                let src = istl_objectstore::ObjectStoreSource::open_url(&url)
                    .with_context(|| format!("opening {:?} from remote", url))?;
                Ok(AnyReader::Remote(Reader::open_source(src)?))
            }
            #[cfg(not(feature = "remote"))]
            Endpoint::Remote(url) => Err(anyhow!(
                "URL input {url:?} requires the `remote` feature"
            )),
        }
    }

    fn query_into(
        &self,
        bbox: &Bbox,
        format: OutputFormat,
        out: &mut dyn Write,
    ) -> std::io::Result<QueryStats> {
        match (self, format) {
            (AnyReader::Local(r), OutputFormat::Stl) => r.query_to_writer(bbox, out),
            (AnyReader::Local(r), OutputFormat::Istl) => r.query_to_writer_istl(bbox, out),
            #[cfg(feature = "remote")]
            (AnyReader::Remote(r), OutputFormat::Stl) => r.query_to_writer(bbox, out),
            #[cfg(feature = "remote")]
            (AnyReader::Remote(r), OutputFormat::Istl) => r.query_to_writer_istl(bbox, out),
        }
    }
}

/// Run `f` against a writer pointed at `output` (path or URL), then commit.
fn with_output_writer<F>(output: Endpoint, f: F) -> Result<QueryStats>
where
    F: FnOnce(&mut dyn Write) -> std::io::Result<QueryStats>,
{
    match output {
        Endpoint::Local(p) => {
            let f_handle = File::create(&p).with_context(|| format!("creating {:?}", p))?;
            let mut out = BufWriter::with_capacity(8 * 1024 * 1024, f_handle);
            let stats = f(&mut out).with_context(|| format!("writing {:?}", p))?;
            out.flush().with_context(|| format!("flushing {:?}", p))?;
            Ok(stats)
        }
        #[cfg(feature = "remote")]
        Endpoint::Remote(url) => {
            let sink = istl_objectstore::ObjectStoreSink::create_url(&url)
                .with_context(|| format!("opening sink at {:?}", url))?;
            // Local std::io::BufWriter amortizes the per-write block_on into the
            // sink (8 MiB matches the existing local path).
            let mut buf = BufWriter::with_capacity(8 * 1024 * 1024, sink);
            let stats = f(&mut buf).with_context(|| format!("writing to {:?}", url))?;
            buf.flush().with_context(|| format!("flushing {:?}", url))?;
            let sink = buf
                .into_inner()
                .map_err(|e| anyhow!("flushing sink {:?}: {e}", url))?;
            sink.finish()
                .with_context(|| format!("finalizing upload to {:?}", url))?;
            Ok(stats)
        }
        #[cfg(not(feature = "remote"))]
        Endpoint::Remote(url) => Err(anyhow!(
            "URL output {url:?} requires the `remote` feature"
        )),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Convert {
            input,
            output,
            group_size,
        } => {
            let stats = convert(&input, &output, group_size)
                .with_context(|| format!("converting {:?} -> {:?}", input, output))?;
            let b = stats.dataset_bbox;
            println!(
                "converted: triangles={} groups={} \
                 dataset_bbox=x[{:.3}..{:.3}] y[{:.3}..{:.3}] z[{:.3}..{:.3}] \
                 footer_offset={} footer_size={}",
                stats.triangles,
                stats.groups,
                b.xmin,
                b.xmax,
                b.ymin,
                b.ymax,
                b.zmin,
                b.zmax,
                stats.footer_offset,
                stats.footer_size,
            );
        }
        Cmd::Query {
            input,
            output,
            bbox,
            format,
        } => {
            let reader = AnyReader::open(classify(&input))?;
            let output_endpoint = classify(&output);
            let stats = with_output_writer(output_endpoint, |w| {
                reader.query_into(&bbox, format, w)
            })?;
            println!(
                "queried: groups_matched={} triangles_written={} format={:?}",
                stats.groups_matched, stats.triangles_written, format
            );
        }
        Cmd::Info { input, sample } => info_cmd(&input, sample)?,
    }
    Ok(())
}

fn info_cmd(input: &Path, sample: usize) -> Result<()> {
    let reader = Reader::open(input)?;
    let groups = &reader.index.groups;
    if groups.is_empty() {
        return Err(anyhow!("index is empty"));
    }
    let mut ds = Bbox::EMPTY;
    for g in groups {
        ds.fold_point(g.bbox.xmin, g.bbox.ymin, g.bbox.zmin);
        ds.fold_point(g.bbox.xmax, g.bbox.ymax, g.bbox.zmax);
    }
    println!(
        "footer_offset={} footer_size={} triangle_count={} groups={}",
        reader.footer_offset,
        reader.footer_size,
        reader.triangle_count,
        groups.len()
    );
    println!(
        "dataset_bbox: x[{:.3}..{:.3}] y[{:.3}..{:.3}] z[{:.3}..{:.3}]",
        ds.xmin, ds.xmax, ds.ymin, ds.ymax, ds.zmin, ds.zmax,
    );
    for (i, g) in groups.iter().take(sample).enumerate() {
        println!(
            "  [{i}] start={} size={} tris={} bbox=x[{:.3}..{:.3}] y[{:.3}..{:.3}] z[{:.3}..{:.3}]",
            g.byte_start,
            g.byte_size,
            g.triangle_count(),
            g.bbox.xmin,
            g.bbox.xmax,
            g.bbox.ymin,
            g.bbox.ymax,
            g.bbox.zmin,
            g.bbox.zmax,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_url_classifies() {
        assert!(looks_like_url("s3://bucket/key"));
        assert!(looks_like_url("https://example.com/x"));
        assert!(looks_like_url("memory:///x"));
        assert!(looks_like_url("file:///tmp/x"));
        assert!(!looks_like_url("/tmp/x"));
        assert!(!looks_like_url("relative/path.istl"));
        assert!(!looks_like_url("C:/Users/me/x.istl"));
        assert!(!looks_like_url(""));
    }

    #[test]
    fn classify_paths_and_urls() {
        match classify("/tmp/x.istl") {
            Endpoint::Local(p) => assert_eq!(p, PathBuf::from("/tmp/x.istl")),
            _ => panic!("expected Local"),
        }
        match classify("s3://b/k") {
            Endpoint::Remote(u) => assert_eq!(u, "s3://b/k"),
            _ => panic!("expected Remote"),
        }
        match classify("file:///tmp/x.istl") {
            Endpoint::Local(p) => assert_eq!(p, PathBuf::from("/tmp/x.istl")),
            _ => panic!("expected Local (file:// stripped)"),
        }
    }
}
