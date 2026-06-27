use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use istl_core::{convert, Reader};

#[derive(Parser)]
#[command(name = "istl", version, about = "Indexed STL converter and reader")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Convert a plain binary STL to a Hilbert-sorted indexed .istl file.
    Convert {
        input: PathBuf,
        output: PathBuf,
        #[arg(long, default_value_t = 8192)]
        group_size: usize,
    },
    /// Query an .istl file with a 3D bbox and emit a plain binary STL.
    /// `--bbox xmin,ymin,zmin,xmax,ymax,zmax`. Use `-inf`/`inf` to disable an axis.
    Query {
        input: PathBuf,
        output: PathBuf,
        #[arg(long, value_parser = parse_bbox)]
        bbox: [f32; 6],
    },
    /// Print summary info about an .istl file's footer.
    Info {
        input: PathBuf,
        #[arg(long, default_value_t = 3)]
        sample: usize,
    },
}

/// Parses "xmin,ymin,zmin,xmax,ymax,zmax" into the canonical bbox6 order
/// [xmin, xmax, ymin, ymax, zmin, zmax] used internally.
fn parse_bbox(s: &str) -> Result<[f32; 6], String> {
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
    Ok([xmin, xmax, ymin, ymax, zmin, zmax])
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
            println!(
                "converted: triangles={} groups={} dataset_bbox=[{:?}] footer_offset={} footer_size={}",
                stats.triangles,
                stats.groups,
                stats.dataset_bbox,
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
            let mut ds = [
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
            ];
            for g in groups {
                for axis in 0..3 {
                    if g.bbox[axis * 2] < ds[axis * 2] {
                        ds[axis * 2] = g.bbox[axis * 2];
                    }
                    if g.bbox[axis * 2 + 1] > ds[axis * 2 + 1] {
                        ds[axis * 2 + 1] = g.bbox[axis * 2 + 1];
                    }
                }
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
                ds[0], ds[1], ds[2], ds[3], ds[4], ds[5]
            );
            for (i, g) in groups.iter().take(sample).enumerate() {
                println!(
                    "  [{i}] start={} size={} tris={} bbox=x[{:.3}..{:.3}] y[{:.3}..{:.3}] z[{:.3}..{:.3}]",
                    g.byte_start,
                    g.byte_size,
                    g.triangle_count(),
                    g.bbox[0], g.bbox[1], g.bbox[2], g.bbox[3], g.bbox[4], g.bbox[5],
                );
            }
        }
    }
    Ok(())
}
