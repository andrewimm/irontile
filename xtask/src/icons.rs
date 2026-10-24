//! Rasterise the site's icon into every shape a browser asks for.
//!
//! One SVG is the source. Everything else here is a rendering of it, so the
//! mark is changed in exactly one place and the rest follows -- which is the
//! whole reason this is a command rather than a folder of PNGs somebody once
//! exported and can no longer reproduce.

use std::fs;
use std::path::{Path, PathBuf};

use resvg::tiny_skia;
use resvg::usvg;

/// Where the icon is drawn, and where each rendering of it belongs.
const SOURCE: &str = "site/app/icon.svg";

/// The sizes packed into `favicon.ico`.
///
/// Three, because that is what a browser picks between: 16 for the tab strip,
/// 32 for the address bar and most desktop shortcuts, 48 for a large one. More
/// sizes only make the file bigger for no one's benefit.
const ICO_SIZES: [u32; 3] = [16, 32, 48];

pub fn run(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(format!("icons takes no arguments, got {args:?}"));
    }
    let root = repo_root()?;
    let svg = root.join(SOURCE);
    let tree = load(&svg)?;

    // One PNG, for the square iOS uses when a page is kept on a home screen.
    // It does its own rounding, so this is drawn square and opaque. There are
    // no larger sizes and no manifest: these are documents to read, not an
    // application to install, and a browser asked to pin them wants a bookmark
    // rather than a window of its own.
    let targets: [(PathBuf, u32); 1] = [(root.join("site/app/apple-icon.png"), 180)];

    let ico = root.join("site/app/favicon.ico");
    let frames: Vec<(u32, Vec<u8>)> = ICO_SIZES
        .iter()
        .map(|&size| render(&tree, size).map(|png| (size, png)))
        .collect::<Result<_, _>>()?;
    write(&ico, &pack_ico(&frames))?;
    println!("{} ({} sizes)", shown(&root, &ico), frames.len());

    for (path, size) in targets {
        let png = render(&tree, size)?;
        write(&path, &png)?;
        println!("{} ({size}x{size})", shown(&root, &path));
    }
    Ok(())
}

/// The workspace root, found from this crate rather than the shell's directory.
fn repo_root() -> Result<PathBuf, String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("no parent of {}", manifest.display()))
}

fn shown(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn load(svg: &Path) -> Result<usvg::Tree, String> {
    let data = fs::read(svg).map_err(|err| format!("reading {}: {err}", svg.display()))?;
    usvg::Tree::from_data(&data, &usvg::Options::default())
        .map_err(|err| format!("parsing {}: {err}", svg.display()))
}

/// One square rendering, as PNG bytes.
fn render(tree: &usvg::Tree, size: u32) -> Result<Vec<u8>, String> {
    let mut pixmap =
        tiny_skia::Pixmap::new(size, size).ok_or_else(|| format!("no pixmap at {size}x{size}"))?;
    let scale = size as f32 / tree.size().width();
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
        .encode_png()
        .map_err(|err| format!("encoding {size}x{size}: {err}"))
}

/// Pack renderings into an ICO.
///
/// The container is a six byte header, then a sixteen byte entry per image,
/// then the images themselves. The entries hold PNG rather than the bitmap the
/// format was written for, which every browser since Vista reads and which
/// keeps the file a tenth of the size.
fn pack_ico(frames: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // an icon, not a cursor
    out.extend_from_slice(&(frames.len() as u16).to_le_bytes());

    // Images follow every entry, so the first one starts after all of them.
    let mut offset = 6 + 16 * frames.len() as u32;
    for (size, png) in frames {
        // 256 is written as zero, which is the only reason this is a byte.
        out.push(if *size >= 256 { 0 } else { *size as u8 });
        out.push(if *size >= 256 { 0 } else { *size as u8 });
        out.push(0); // no palette
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in frames {
        out.extend_from_slice(png);
    }
    out
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("creating {}: {err}", parent.display()))?;
    }
    fs::write(path, bytes).map_err(|err| format!("writing {}: {err}", path.display()))
}

#[cfg(test)]
mod packing {
    use super::*;

    fn entry(ico: &[u8], n: usize) -> (u8, u32, u32) {
        let at = 6 + 16 * n;
        let size = u32::from_le_bytes(ico[at + 8..at + 12].try_into().unwrap());
        let offset = u32::from_le_bytes(ico[at + 12..at + 16].try_into().unwrap());
        (ico[at], size, offset)
    }

    #[test]
    fn the_header_counts_what_it_holds() {
        let ico = pack_ico(&[(16, vec![1, 2, 3]), (32, vec![4, 5])]);
        assert_eq!(&ico[0..2], &[0, 0], "reserved");
        assert_eq!(
            u16::from_le_bytes([ico[2], ico[3]]),
            1,
            "an icon, not a cursor"
        );
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]), 2, "two images");
    }

    #[test]
    fn every_image_is_found_where_its_entry_says() {
        let frames = vec![(16, vec![1u8, 2, 3]), (32, vec![4u8, 5]), (48, vec![6u8])];
        let ico = pack_ico(&frames);
        for (n, (size, png)) in frames.iter().enumerate() {
            let (width, len, offset) = entry(&ico, n);
            assert_eq!(width as u32, *size);
            assert_eq!(len as usize, png.len());
            let at = offset as usize;
            assert_eq!(&ico[at..at + png.len()], png.as_slice(), "image {n}");
        }
        // Nothing before the first image but the header and the entries.
        assert_eq!(entry(&ico, 0).2, 6 + 16 * 3);
    }

    #[test]
    fn two_hundred_and_fifty_six_is_written_as_nothing() {
        // The field is one byte, so the largest icon a container can describe
        // is recorded as zero rather than as a number that will not fit.
        let ico = pack_ico(&[(256, vec![7])]);
        assert_eq!(entry(&ico, 0).0, 0);
    }
}
