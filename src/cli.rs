// SPDX-License-Identifier: GPL-3.0-or-later
//! Use from a terminal, without the window.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::hdri::{self, HdrFormat};
use crate::links;
use crate::pano::{self, Control, Format, Options, Step};
use crate::service::Client;

pub const HELP: &str = "\
OpenView - 360 panoramas and photos from map applications, at full resolution

Without arguments the window opens.

  OpenView --window <link or 360 image>...
                           Open the window with these links already added; an image on
                           disk opens in the HDRI converter instead

Download:
  OpenView <link> [<link>...] [options]
    -o, --out <folder>     Destination folder (default: the current one)
    -f, --format <type>    jpg (default, quality 97), png or tif
    -n, --north            Turn the image so its centre looks north
    -l, --list <file>      Read links from a text file, one per line (# starts a comment)
    -j, --json             Also write a .json with position, date, heading, author and link
        --max-width <px>   Download a smaller version, not wider than this
        --cover-ground     Cover the ground under the camera with a patch
        --no-level         Do not straighten the horizon
        --no-complete      Do not complete 360 photos that do not cover the whole sphere

Convert a 360 image to HDRI:
  OpenView hdri <image> [options]
        --sun <ratio>      Light of the sun compared with the sky (default 5; clear 4-8, haze 1-3)
        --no-sun           Overcast day: no sun
        --sun-size <deg>   Radius of the sun in degrees (default 1)
        --sun-at <x,y>     Where the sun is, from 0 to 1 across and down the image
        --highlights <x>   Boost of the sky and highlights (default 3; 1 = none)
        --exposure <ev>    Overall exposure in stops (default 0)
        --res <px>         Width of the HDRI (default 8192, or original; never larger than the image)
        --format <type>    exr (default) or hdr
    -o, --out <file>       File to write (default: next to the image, ending in _hdri)

  -h, --help               This text
  -V, --version            Version
";

#[derive(Debug, PartialEq)]
pub enum Command {
    Help,
    Version,
    Download { links: Vec<String>, lists: Vec<PathBuf>, options: Options },
    Hdri { image: PathBuf, params: hdri::Params, width: Option<u32>, format: HdrFormat, out: Option<PathBuf> },
}

fn value<'a>(args: &'a [String], i: &mut usize, option: &str) -> Result<&'a str, String> {
    *i += 1;
    args.get(*i).map(String::as_str).ok_or_else(|| format!("{option} needs a value"))
}

fn number<T: std::str::FromStr>(text: &str, option: &str) -> Result<T, String> {
    text.parse().map_err(|_| format!("{option}: '{text}' is not a valid number"))
}

/// Reads the command line (without the program's name).
pub fn parse(args: &[String]) -> Result<Command, String> {
    if args.iter().any(|a| a == "-h" || a == "--help" || a == "/?") {
        return Ok(Command::Help);
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        return Ok(Command::Version);
    }
    if args.first().map(String::as_str) == Some("hdri") {
        return parse_hdri(&args[1..]);
    }
    let mut options = Options::default();
    let (mut links, mut lists) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-o" | "--out" => options.out_dir = PathBuf::from(value(args, &mut i, arg)?),
            "-f" | "--format" => {
                let text = value(args, &mut i, arg)?;
                options.format = Format::parse(text).ok_or_else(|| format!("unknown format '{text}': use jpg, png or tif"))?;
            }
            "-n" | "--north" => options.north = true,
            "-l" | "--list" => lists.push(PathBuf::from(value(args, &mut i, arg)?)),
            "-j" | "--json" => options.json = true,
            "--max-width" => options.max_width = Some(number(value(args, &mut i, arg)?, arg)?),
            "--cover-ground" => options.cover_ground = true,
            "--no-level" => options.level = false,
            "--no-complete" => options.complete = false,
            _ if arg.starts_with('-') && arg.len() > 1 && !arg[1..].starts_with(|c: char| c.is_ascii_digit()) => {
                return Err(format!("unknown option {arg}"));
            }
            _ => links.push(arg.to_string()),
        }
        i += 1;
    }
    if links.is_empty() && lists.is_empty() {
        return Err("no link given".into());
    }
    Ok(Command::Download { links, lists, options })
}

fn parse_hdri(args: &[String]) -> Result<Command, String> {
    let mut params = hdri::Params::default();
    let (mut image, mut out) = (None, None);
    let mut width = Some(8192);
    let mut format = HdrFormat::Exr;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--sun" => params.sun_ratio = number(value(args, &mut i, arg)?, arg)?,
            "--no-sun" => params.sun_ratio = 0.0,
            "--sun-size" => params.sun_size = number(value(args, &mut i, arg)?, arg)?,
            "--highlights" => params.highlights = number(value(args, &mut i, arg)?, arg)?,
            "--exposure" => params.exposure = number(value(args, &mut i, arg)?, arg)?,
            "--sun-at" => {
                let text = value(args, &mut i, arg)?;
                let (x, y) = text.split_once(',').ok_or_else(|| format!("{arg} needs x,y"))?;
                params.sun_pos = Some((number(x.trim(), arg)?, number(y.trim(), arg)?));
            }
            "--res" => {
                let text = value(args, &mut i, arg)?;
                width = if text.eq_ignore_ascii_case("original") { None } else { Some(number(text, arg)?) };
            }
            "--format" | "-f" => {
                let text = value(args, &mut i, arg)?;
                format = HdrFormat::parse(text).ok_or_else(|| format!("unknown format '{text}': use exr or hdr"))?;
            }
            "-o" | "--out" => out = Some(PathBuf::from(value(args, &mut i, arg)?)),
            _ if arg.starts_with("--") => return Err(format!("unknown option {arg}")),
            _ if image.is_none() => image = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument {arg}")),
        }
        i += 1;
    }
    let image = image.ok_or("no image given")?;
    Ok(Command::Hdri { image, params: params.clamped(), width, format, out })
}

/// Runs a command line and returns the exit code.
pub fn run(args: &[String]) -> i32 {
    match parse(args) {
        Ok(Command::Help) => {
            println!("{HELP}");
            0
        }
        Ok(Command::Version) => {
            println!("OpenView {}", crate::VERSION);
            0
        }
        Ok(Command::Download { links, lists, options }) => run_download(links, &lists, &options),
        Ok(Command::Hdri { image, params, width, format, out }) => run_hdri(&image, &params, width, format, out),
        Err(message) => {
            eprintln!("OpenView: {message}\nTry: OpenView --help");
            2
        }
    }
}

fn run_download(mut links: Vec<String>, lists: &[PathBuf], options: &Options) -> i32 {
    for list in lists {
        match fs::read_to_string(list) {
            Ok(text) => links.extend(links::split_links(&text)),
            Err(e) => {
                eprintln!("could not read {}: {e}", list.display());
                return 2;
            }
        }
    }
    let client = Client::new();
    let cancel = AtomicBool::new(false);
    let progress = |step: Step| {
        print!("\r  {:<40}", step.label());
        let _ = std::io::stdout().flush();
    };
    let control = Control { cancel: &cancel, progress: &progress };
    let mut failed = 0;
    for link in &links {
        println!("{link}");
        let result = match client.find(link) {
            Ok(info) => {
                let (w, h) = info.sizes.get(info.zoom_for(options.max_width)).copied().unwrap_or((0, 0));
                let kind = match (info.flat, info.kind) {
                    (true, _) => "photo",
                    (false, links::Kind::Official) => "street-level panorama",
                    (false, links::Kind::User) => "user panorama",
                };
                println!(
                    "  {kind}{}{}{}{}",
                    if w > 0 { format!(", {w} x {h}") } else { String::new() },
                    if info.date.is_some() { format!(", {}", info.date_text()) } else { String::new() },
                    info.address.as_ref().map(|a| format!(", {}", a.split(" / ").next().unwrap_or(a))).unwrap_or_default(),
                    info.author.as_ref().map(|a| format!(", by {a}")).unwrap_or_default(),
                );
                pano::download(&client, &info, link, options, &control)
            }
            Err(e) => Err(pano::Error::Service(e)),
        };
        match result {
            Ok(saved) => {
                println!("\r  saved {} ({} x {})", saved.path.display(), saved.size.0, saved.size.1);
                if saved.missing_tiles > 0 {
                    println!("  warning: the service did not have {} tiles; they are black", saved.missing_tiles);
                }
            }
            Err(e) => {
                println!("\r  failed: {e:<40}");
                failed += 1;
            }
        }
    }
    i32::from(failed > 0)
}

fn run_hdri(image: &Path, params: &hdri::Params, width: Option<u32>, format: HdrFormat, out: Option<PathBuf>) -> i32 {
    let source = match hdri::load(image) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("{}: {e}", image.display());
            return 1;
        }
    };
    let out = out.unwrap_or_else(|| hdri::output_path(image, format));
    let small = hdri::analysis_copy(&source.rgb);
    let model = hdri::Model::new(&small, params);
    match model.sun_used {
        Some((x, y)) => println!("sun at {x:.3},{y:.3} (ratio {}, radius {} degrees, +{:.1} EV)", params.sun_ratio, params.sun_size, model.gain.log2()),
        None => println!("no sun"),
    }
    let cancel = AtomicBool::new(false);
    let progress = |f: f32| {
        print!("
  {:5.1}%", f * 100.0);
        let _ = std::io::stdout().flush();
    };
    match hdri::write(&source, params, width, format, &out, &cancel, &progress) {
        Ok((w, h)) => {
            println!("
saved {} ({w} x {h})", out.display());
            0
        }
        Err(e) => {
            eprintln!("failed: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn download_options_are_read() {
        let command = parse(&args("https://a.example/x -o out -f png -n -j --max-width 8192 --cover-ground --no-level --no-complete -l list.txt")).unwrap();
        let Command::Download { links, lists, options } = command else { panic!("not a download") };
        assert_eq!(links, vec!["https://a.example/x"]);
        assert_eq!(lists, vec![PathBuf::from("list.txt")]);
        assert_eq!(
            options,
            Options {
                out_dir: PathBuf::from("out"),
                format: Format::Png,
                north: true,
                level: false,
                cover_ground: true,
                complete: false,
                json: true,
                max_width: Some(8192),
            }
        );
    }

    #[test]
    fn defaults_match_the_window() {
        let Command::Download { options, .. } = parse(&args("link")).unwrap() else { panic!() };
        assert_eq!(options, Options::default());
        assert!(options.level && options.complete && !options.north && !options.json);
    }

    #[test]
    fn coordinates_with_a_minus_sign_are_links() {
        let Command::Download { links, .. } = parse(&["-33.85,151.21".to_string()]).unwrap() else { panic!() };
        assert_eq!(links, vec!["-33.85,151.21"]);
    }

    #[test]
    fn hdri_options_are_read() {
        let command = parse(&args("hdri pano.jpg --sun 8 --sun-size 2 --res 4096 --format hdr")).unwrap();
        let Command::Hdri { image, params, width, format, out } = command else { panic!("not hdri") };
        assert_eq!(image, PathBuf::from("pano.jpg"));
        assert_eq!((params.sun_ratio, params.sun_size, params.sun_pos), (8.0, 2.0, None));
        assert_eq!((width, format, out), (Some(4096), HdrFormat::Hdr, None));
        let Command::Hdri { params, width, .. } = parse(&args("hdri p.png --no-sun --res original")).unwrap() else { panic!() };
        assert_eq!((params.sun_ratio, width), (0.0, None));
        let Command::Hdri { params, .. } = parse(&args("hdri p.png --sun-at 0.25,0.3")).unwrap() else { panic!() };
        assert_eq!(params.sun_pos, Some((0.25, 0.3)));
        let Command::Hdri { params, .. } = parse(&args("hdri p.png --highlights 5 --exposure -1")).unwrap() else { panic!() };
        assert_eq!((params.highlights, params.exposure), (5.0, -1.0));
    }

    #[test]
    fn mistakes_are_explained() {
        assert_eq!(parse(&args("--frobnicate x")), Err("unknown option --frobnicate".into()));
        assert_eq!(parse(&args("link -f bmp")), Err("unknown format 'bmp': use jpg, png or tif".into()));
        assert_eq!(parse(&args("link -o")), Err("-o needs a value".into()));
        assert_eq!(parse(&args("-n")), Err("no link given".into()));
        assert_eq!(parse(&args("hdri")), Err("no image given".into()));
        assert_eq!(parse(&args("--help")), Ok(Command::Help));
        assert_eq!(parse(&args("x --version")), Ok(Command::Version));
    }
}
