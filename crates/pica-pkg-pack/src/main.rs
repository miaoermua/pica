mod archive;
mod backend;
mod build;
mod container;
mod matrix;
mod rewrite;

use build::main_build;
use pica_pkg_core::PICA_VERSION;
use pica_pkg_core::error::{PicaError, PicaResult};
use std::env;
use std::path::{Path, PathBuf};
use std::process;

fn usage() {
  println!(
    "Usage:\n  pica-pack build <pkgdir> [--outdir DIR] [--pkgmgr opkg|apk|pica|opkg,apk]\n\
     \n\
     <pkgdir> layout:\n  \
       manifest                         required, key = value (see docs/standard.md)\n  \
       scripts/                         optional, lifecycle scripts (install/update/remove/.env)\n  \
       root/                            optional, files mirrored onto the target rootfs\n  \
       pkgs/<backend>/                  optional, containers for that backend; pica unpacks them itself\n  \
       pkgs/<backend>/<platform>/<arch>/  optional matrix layout\n  \
       LICENSE                          optional\n\
     \n\
     Build targets come from `targets` in the manifest (repeatable or\n\
     comma-separated), overridden by --pkgmgr. Defaults to opkg.\n\
     \n\
     Each <platform>/<arch> combination produces one artifact holding only\n\
     that combination's packages, flattened into pkgs/.\n\
     \n\
     Artifact filename:\n  \
       <pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>[-apk|-pica].pkg.tar.gz\n  \
       (the platform segment is omitted when platform = all)\n\
     \n\
     Output (one directory per backend):\n  \
       opkg -> <outdir>/opkg/    apk -> <outdir>/apk/    pica -> <outdir>/pica/\n  \
       each with its own repo.json and SHA256SUMS"
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
      let Some(pkgdir_arg) = args.next() else {
        return Err(PicaError::msg("build requires <pkgdir>"));
      };

      let mut outdir: Option<PathBuf> = None;
      let mut pkgmgr: Option<String> = None;
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
          "--pkgmgr" => {
            let Some(value) = rest.get(index + 1) else {
              return Err(PicaError::msg(
                "--pkgmgr requires opkg, apk, pica, or a comma-separated list",
              ));
            };
            pkgmgr = Some(value.clone());
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

      main_build(Path::new(&pkgdir_arg), outdir, pkgmgr.as_deref())
    }
    other => Err(PicaError::msg(format!("unknown command: {other}"))),
  }
}
