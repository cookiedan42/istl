//! Command-line interface to the `istl` crate.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use istl_core::{Bbox, DEFAULT_GROUP_SIZE, Reader, convert};

#[derive(Parser)]
#[command(name = "istl", version, about = "Indexed STL converter and reader")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
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
    /// Query an .istl file with a 3D bbox and emit a plain binary STL.
    /// `--bbox xmin,ymin,zmin,xmax,ymax,zmax`. Use `-inf`/`inf` to disable an axis.
    Query {
        input: PathBuf,
        output: PathBuf,
        #[arg(long, value_parser = parse_bbox)]
        bbox: Bbox,
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
        } => {
            let reader = Reader::open(&input)?;
            let stats = reader
                .query(&bbox, &output)
                .with_context(|| format!("querying {:?} -> {:?}", input, output))?;
            println!(
                "queried: groups_matched={} triangles_written={}",
                stats.groups_matched, stats.triangles_written
            );
        }
        Cmd::Info { input, sample } => {
            let reader = Reader::open(&input)?;
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
        }
    }
    Ok(())
}
