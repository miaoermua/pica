mod archive;
mod build;
mod platform;
mod rewrite;

use build::main_build;
use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::PICA_VERSION;
use std::env;
use std::path::{Path, PathBuf};
use std::process;

fn usage() {
  println!(
        "Usage:\n  pica-pack build <staging_dir> [--outdir DIR]\n\
\nstaging_dir must contain:\n  - manifest\n  - cmd/\n  - binary/ is optional\n  - src/ is optional\n  - depend/ is optional\n  - LICENSE is optional\n\
\nIf binary/ exists, the recommended layout is:\n  binary/<platform>/<arch>/*.ipk\n\
\nIf depend/ exists, the recommended layout is:\n  depend/<platform>/<arch>/*.ipk\n\
\nWhen such layout is present, pica-pack will build one package per\n<platform>/<arch> combination.\n\
\nPackage filename:\n  <pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>.pkg.tar.gz"
    );
}

fn msg(text: impl AsRef<str>) {
  println!("==> {}", text.as_ref());
}

fn msg2(text: impl AsRef<str>) {
  println!("  -> {}", text.as_ref());
}

fn main() {
  if let Err(err) = run() {
    eprintln!("pica-pack: {err}");
    process::exit(1);
  }
}

fn run() -> PicaResult<()> {
  let mut args = env::args().skip(1);
  let Some(command) = args.next() else {
    usage();
    return Err(PicaError::msg("missing command"));
  };

  match command.as_str() {
    "-h" | "--help" | "help" => {
      usage();
      Ok(())
    }
    "--version" => {
      println!("{PICA_VERSION}");
      Ok(())
    }
    "build" => {
      let Some(staging_dir_arg) = args.next() else {
        return Err(PicaError::msg("build requires <staging_dir>"));
      };

      let mut outdir: Option<PathBuf> = None;
      let rest: Vec<String> = args.collect();
      let mut index = 0;
      while index < rest.len() {
        match rest[index].as_str() {
          "--outdir" => {
            let Some(value) = rest.get(index + 1) else {
              return Err(PicaError::msg("--outdir requires DIR"));
            };
            outdir = Some(PathBuf::from(value));
            index += 2;
          }
          "-h" | "--help" => {
            usage();
            return Ok(());
          }
          other => {
            return Err(PicaError::msg(format!("unknown arg: {other}")));
          }
        }
      }

      main_build(Path::new(&staging_dir_arg), outdir)
    }
    other => Err(PicaError::msg(format!("unknown command: {other}"))),
  }
}
