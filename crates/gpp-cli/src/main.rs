//! `gpp`: command-line front end of gemini-pack-patcher.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gpp_core::engine::{SelectedPatch, Selection};
use gpp_core::{catalog, FirmwareDef};

#[derive(Parser)]
#[command(name = "gpp", version, about = "Patch stock GEMINI_PACK.BIN head unit firmware")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List supported firmware versions and their patches.
    List,
    /// Identify a firmware file by its SHA-256.
    Identify { file: PathBuf },
    /// Apply patches to a stock firmware file.
    Apply {
        /// The original, unmodified GEMINI_PACK.BIN.
        file: PathBuf,
        /// Output file (put it on a USB stick as GEMINI_PACK.BIN).
        #[arg(short, long, default_value = "patched/GEMINI_PACK.BIN")]
        output: PathBuf,
        /// Patch to apply (repeatable), see `gpp list`.
        #[arg(short, long = "patch", value_name = "ID", required = true)]
        patches: Vec<String>,
        /// Patch parameter as PATCH.PARAM=VALUE, e.g. --set "wifi-name.name=My Car".
        #[arg(short, long = "set", value_name = "PATCH.PARAM=VALUE")]
        params: Vec<String>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run the update container's integrity checks on any GEMINI_PACK.BIN.
    Verify { file: PathBuf },
    /// Download a supported stock firmware from the vendor and check its hash.
    Download {
        /// Firmware id from `gpp list`.
        id: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Look for firmware versions on the vendor server that this tool does not know yet.
    CheckUpdates,
    /// Write every partition of a pack to DIR (for research and porting).
    Extract { file: PathBuf, dir: PathBuf },
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn run(cmd: Command) -> Res<ExitCode> {
    match cmd {
        Command::List => {
            for f in catalog() {
                println!("{}  ({})\n  sha256 {}", f.name, f.id, f.sha256);
                if let Some(u) = &f.download_url {
                    println!("  download: {u}");
                }
                for p in &f.patches {
                    println!("  - {:<14} {}", p.id, p.title);
                    for q in &p.params {
                        println!("      --set {}.{}=...  {} ({})", p.id, q.id, q.label, q.help);
                    }
                }
                println!();
            }
        }
        Command::Identify { file } => {
            let id = gpp_core::identify(&std::fs::read(&file)?);
            println!("sha256 {}", id.sha256);
            match id.firmware {
                Some(f) => println!("supported stock firmware: {} ({})", f.name, f.id),
                None => {
                    println!("not a supported stock firmware");
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        Command::Apply {
            file,
            output,
            patches,
            params,
            json,
        } => {
            if output.exists() && same_file(&file, &output) {
                return Err("output would overwrite the input file".into());
            }
            let mut values: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
            for s in &params {
                let (key, value) = s
                    .split_once('=')
                    .ok_or_else(|| format!("--set {s}: expected PATCH.PARAM=VALUE"))?;
                let (patch, param) = key
                    .split_once('.')
                    .ok_or_else(|| format!("--set {s}: expected PATCH.PARAM=VALUE"))?;
                if !patches.iter().any(|p| p == patch) {
                    return Err(format!("--set {s}: patch '{patch}' is not selected (add -p {patch})").into());
                }
                values
                    .entry(patch.into())
                    .or_default()
                    .insert(param.into(), value.into());
            }
            let sel = Selection {
                patches: patches
                    .iter()
                    .map(|id| SelectedPatch {
                        id: id.clone(),
                        params: values.remove(id).unwrap_or_default(),
                    })
                    .collect(),
            };
            let input = std::fs::read(&file)?;
            let mut last = String::new();
            let out = gpp_core::apply(&input, &sel, &mut |stage, _| {
                if !json && stage != last && stage != "Done" {
                    eprintln!("{stage}...");
                    last = stage.to_string();
                }
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out.report)?);
            } else {
                println!("\nApplied: {}", out.report.applied.join(", "));
                for c in &out.report.checks {
                    println!("  [{}] {}: {}", if c.ok { " ok " } else { "FAIL" }, c.name, c.detail);
                }
            }
            if !out.report.ok {
                return Err("verification failed; no output written".into());
            }
            if let Some(dir) = output.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&output, &out.data)?;
            if !json {
                println!("\nWrote {} (sha256 {})", output.display(), out.report.output_sha256);
                println!("Copy it to the root of a FAT32 USB stick as GEMINI_PACK.BIN and run the system update.");
            }
        }
        Command::Verify { file } => {
            let data = std::fs::read(&file)?;
            let checks = gpp_core::pack::validate(&data);
            for c in &checks {
                println!("[{}] {}: {}", if c.ok { " ok " } else { "FAIL" }, c.name, c.detail);
            }
            if !checks.iter().all(|c| c.ok) {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Download { id, output } => {
            let f = find(&id)?;
            let url = f
                .download_url
                .as_deref()
                .ok_or("no download URL known for this firmware")?;
            let output = output.unwrap_or_else(|| PathBuf::from(format!("{}/GEMINI_PACK.BIN", f.id)));
            if output.exists() {
                return Err(format!("{} already exists", output.display()).into());
            }
            eprintln!("Downloading {url}");
            let data = download(url, f.size)?;
            let got = gpp_core::identify(&data);
            if got.firmware.map(|x| &x.id) != Some(&f.id) {
                return Err(format!(
                    "downloaded file does not match (sha256 {}); the vendor file may have changed",
                    got.sha256
                )
                .into());
            }
            if let Some(dir) = output.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&output, &data)?;
            println!("Saved {} (sha256 verified)", output.display());
        }
        Command::Extract { file, dir } => {
            let data = std::fs::read(&file)?;
            let layout = gpp_core::pack::Layout::parse(&data)?;
            std::fs::create_dir_all(&dir)?;
            for p in &layout.partitions {
                let path = dir.join(format!("{}.bin", p.name));
                std::fs::write(&path, layout.read(&data, &p.name)?)?;
                println!("{:<14} {:>10} bytes  {}", p.name, p.size(), path.display());
            }
            let text = &data[layout.script_text.0..layout.script_text.1];
            std::fs::write(dir.join("main_script.txt"), text)?;
            println!(
                "Unpack a SquashFS partition with e.g.: unsquashfs -d {}/spapp {}/spapp.bin",
                dir.display(),
                dir.display()
            );
        }
        Command::CheckUpdates => {
            let mut indexes: Vec<&str> = catalog().iter().filter_map(|f| f.update_index.as_deref()).collect();
            indexes.dedup();
            let mut unknown = 0;
            for index in indexes {
                println!("{index}");
                let html = ureq::get(index).call()?.body_mut().read_to_string()?;
                for name in listing_dirs(&html) {
                    let url = format!(
                        "{}{}/GEMINI_PACK.BIN",
                        index.trim_end_matches('/').to_owned() + "/",
                        name
                    );
                    let known = catalog()
                        .iter()
                        .any(|f| f.download_url.as_deref() == Some(url.as_str()));
                    println!(
                        "  {:<24} {}",
                        name,
                        if known { "supported" } else { "NEW - not supported yet" }
                    );
                    unknown += usize::from(!known);
                }
            }
            if unknown > 0 {
                println!("\nA new version needs a firmware definition, see docs/porting.md.");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn find(id: &str) -> Res<&'static FirmwareDef> {
    catalog()
        .iter()
        .find(|f| f.id == id)
        .ok_or_else(|| format!("unknown firmware id '{id}' (see gpp list)").into())
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

fn download(url: &str, size: u64) -> Res<Vec<u8>> {
    let mut resp = ureq::get(url).call()?;
    let mut reader = resp.body_mut().as_reader();
    let mut data = Vec::with_capacity(size as usize);
    let mut buf = vec![0u8; 1 << 16];
    let mut shown = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if data.len() as u64 > size {
            return Err("download is larger than expected".into());
        }
        let pct = data.len() as u64 * 100 / size.max(1);
        if pct >= shown + 5 {
            shown = pct;
            eprint!("\r  {pct:>3}%");
            std::io::stderr().flush().ok();
        }
    }
    eprintln!();
    Ok(data)
}

/// Sub-directory names from an IIS-style directory listing.
fn listing_dirs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in html.split("<A HREF=\"").chain(html.split("<a href=\"")).skip(1) {
        let Some(href) = part.split('"').next() else { continue };
        if let Some(name) = href.strip_suffix('/').and_then(|h| h.rsplit('/').next()) {
            if !name.is_empty() && !href.ends_with("../") && !out.contains(&name.to_string()) {
                out.push(name.to_string());
            }
        }
    }
    out.retain(|n| html.contains(&format!(">{n}<")));
    out
}
