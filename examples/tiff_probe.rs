//! Decode every page of a TIFF file and print what came out.
//!
//! ```sh
//! cargo run --release --example tiff_probe -- in.tif [out-prefix]
//! ```
//!
//! With an output prefix the native sample plane of page `i` is written
//! to `<prefix>.<i>.raw` (the layout printed for that page).

use std::fs;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: tiff_probe <file.tif> [out-prefix]");
        std::process::exit(2);
    };
    let prefix = args.next();
    let bytes = fs::read(&path).expect("read input");
    match oxideav_tiff::decode_pages(&bytes) {
        Ok(pages) => {
            for (i, page) in pages.iter().enumerate() {
                let l = &page.layout;
                println!(
                    "page {i}: {}x{} {:?} (photometric {:?}, compression {:?}, bits {:?}, \
                     samples {}, planar {:?}, predictor {:?}, tiled {})",
                    page.width(),
                    page.height(),
                    page.format(),
                    l.photometric,
                    l.compression,
                    l.bits_per_sample,
                    l.samples_per_pixel,
                    l.planar_config,
                    l.predictor,
                    l.tiled,
                );
                if let (Some(prefix), Some(data)) = (&prefix, page.image.as_bytes()) {
                    fs::write(format!("{prefix}.{i}.raw"), data).expect("write plane");
                }
            }
        }
        Err(e) => {
            println!("error: {e}");
            std::process::exit(1);
        }
    }
}
