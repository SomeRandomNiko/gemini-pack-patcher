//! Tests against real firmware files, which are not part of the repository.
//!
//! Set `GPP_STOCK` to the stock AR560R4 `GEMINI_PACK.BIN` to run them, and optionally
//! `GPP_REFERENCE` to a pack built by the original Python tool with the name
//! "S3 Niko" (the build that was verified in the car).

use gpp_core::engine::{SelectedPatch, Selection};
use gpp_core::pack::Layout;
use gpp_core::squashfs::{write::rebuild, Image, Kind};

fn stock() -> Option<Vec<u8>> {
    let p = std::env::var("GPP_STOCK").ok()?;
    Some(std::fs::read(p).expect("GPP_STOCK not readable"))
}

fn select(names: Option<(&str, &str)>) -> Selection {
    let mut patches = vec![SelectedPatch {
        id: "aa-keep-alive".into(),
        params: Default::default(),
    }];
    if let Some((wifi, bt)) = names {
        patches.push(SelectedPatch {
            id: "wifi-name".into(),
            params: [("name".into(), wifi.into())].into(),
        });
        patches.push(SelectedPatch {
            id: "bt-name".into(),
            params: [("name".into(), bt.into())].into(),
        });
    }
    Selection { patches }
}

#[test]
fn identity_rebuild_is_lossless_and_same_size() {
    let Some(data) = stock() else { return };
    let layout = Layout::parse(&data).unwrap();
    let raw = layout.read(&data, "spapp").unwrap();
    let img = Image::open(&raw).unwrap();
    let (new, stats) = rebuild(&img, &img.fs).unwrap();
    eprintln!(
        "original bytes_used {} new {} ({:?})",
        img.fs.sb.bytes_used, stats.bytes_used, stats
    );
    let re = Image::open(&new).unwrap();
    let a = img.fs.walk();
    let b = re.fs.walk();
    assert_eq!(a.len(), b.len());
    for ((pa, na), (pb, nb)) in a.iter().zip(&b) {
        assert_eq!(pa, pb);
        if let (Kind::File { layout: Some(x), .. }, Kind::File { layout: Some(y), .. }) =
            (&img.fs.nodes[*na].kind, &re.fs.nodes[*nb].kind)
        {
            assert_eq!(img.read_layout(x).unwrap(), re.read_layout(y).unwrap(), "{pa}");
        }
    }
    // Only the metadata is recompressed; allow a small difference.
    assert!(stats.bytes_used.abs_diff(img.fs.sb.bytes_used) < 2048);
}

#[test]
fn apply_all_patches() {
    let Some(data) = stock() else { return };
    let out = gpp_core::apply(&data, &select(Some(("S3 Niko", "S3 Niko"))), &mut |s, f| {
        eprintln!("{f:.2} {s}")
    })
    .unwrap();
    for c in &out.report.checks {
        eprintln!("[{}] {}: {}", if c.ok { "ok" } else { "FAIL" }, c.name, c.detail);
    }
    assert!(out.report.ok);
    if let Ok(dir) = std::env::var("GPP_OUT") {
        std::fs::write(format!("{dir}/all.BIN"), &out.data).unwrap();
    }

    // Same file contents as the reference build.
    let Ok(reference) = std::env::var("GPP_REFERENCE") else {
        return;
    };
    let reference = std::fs::read(reference).unwrap();
    let (la, lb) = (Layout::parse(&out.data).unwrap(), Layout::parse(&reference).unwrap());
    for p in &la.partitions {
        if p.name != "spapp" {
            assert_eq!(
                la.read(&out.data, &p.name).unwrap(),
                lb.read(&reference, &p.name).unwrap(),
                "{}",
                p.name
            );
        }
    }
    let (ra, rb) = (
        la.read(&out.data, "spapp").unwrap(),
        lb.read(&reference, "spapp").unwrap(),
    );
    let (a, b) = (Image::open(&ra).unwrap(), Image::open(&rb).unwrap());
    let (wa, wb) = (a.fs.walk(), b.fs.walk());
    assert_eq!(
        wa.iter().map(|x| &x.0).collect::<Vec<_>>(),
        wb.iter().map(|x| &x.0).collect::<Vec<_>>()
    );
    for ((path, na), (_, nb)) in wa.iter().zip(&wb) {
        let (x, y) = (&a.fs.nodes[*na], &b.fs.nodes[*nb]);
        assert_eq!(x.permissions, y.permissions, "{path}");
        if let (Kind::File { .. }, Kind::File { .. }) = (&x.kind, &y.kind) {
            assert_eq!(a.content(&a.fs, *na).unwrap(), b.content(&b.fs, *nb).unwrap(), "{path}");
        }
    }
}

#[test]
fn only_keep_alive_and_only_bt() {
    let Some(data) = stock() else { return };
    let out = gpp_core::apply(&data, &select(None), &mut |_, _| {}).unwrap();
    assert!(out.report.ok);
    let bt = Selection {
        patches: vec![SelectedPatch {
            id: "bt-name".into(),
            params: [("name".into(), "Golf".into())].into(),
        }],
    };
    let out = gpp_core::apply(&data, &bt, &mut |_, _| {}).unwrap();
    assert!(out.report.ok);
    let l = Layout::parse(&out.data).unwrap();
    let raw = l.read(&out.data, "spapp").unwrap();
    let img = Image::open(&raw).unwrap();
    let script = img.content(&img.fs, img.fs.lookup("bin/devname.sh").unwrap()).unwrap();
    assert!(String::from_utf8(script).unwrap().contains("C='BtName=Golf'\n"));
}

#[test]
fn rejects_bad_input() {
    let Some(data) = stock() else { return };
    let bad = Selection {
        patches: vec![SelectedPatch {
            id: "wifi-name".into(),
            params: [("name".into(), "a'b".into())].into(),
        }],
    };
    assert!(gpp_core::apply(&data, &bad, &mut |_, _| {}).is_err());
    let mut modified = data.clone();
    modified[0x5000000] ^= 1;
    assert!(gpp_core::apply(&modified, &select(None), &mut |_, _| {}).is_err());
}
