//! A one-question menu for `pica setup`.
//!
//! On a terminal the arrow keys move a cursor and Enter answers, drawn in place
//! with escape sequences. Everywhere else - a pipe, a file, a CI log - the same
//! options are numbered and the answer is typed, so the wizard never hangs
//! waiting for keys nobody can press. Both paths accept a bare Enter as "the
//! default", which is what makes the happy path one key press per question.
//!
//! No dependency is involved: the terminal is put into character-at-a-time mode
//! with `stty` and restored from a guard, so a cancel, an error or a panic on
//! the way out cannot leave the shell without echo.

use crate::app::App;
use crate::i18n::Key;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::process::{Command, Stdio};

/// Asks `prompt` and returns the chosen index, or `None` when the user
/// cancelled or input ran out.
///
/// `default` is what a bare Enter answers; it is also where the cursor starts.
pub(crate) fn select(app: &App, prompt: &str, options: &[String], default: usize) -> Option<usize> {
  if options.is_empty() {
    return None;
  }
  let default = default.min(options.len() - 1);
  println!("{prompt}");

  match Terminal::enter() {
    Some(terminal) => {
      let picked = raw(options, default);
      drop(terminal);
      picked
    }
    None => numbered(app, options, default),
  }
}

/// Reads one line, or `None` at end of input.
pub(crate) fn line(prompt: &str) -> Option<String> {
  print!("{prompt}");
  io::stdout().flush().ok()?;

  let mut text = String::new();
  match io::stdin().lock().read_line(&mut text) {
    Ok(0) | Err(_) => None,
    Ok(_) => Some(text.trim_end_matches(['\r', '\n']).to_string()),
  }
}

/// The numbered fallback: no cursor, no redraw, no terminal state to restore.
fn numbered(app: &App, options: &[String], default: usize) -> Option<usize> {
  for (index, option) in options.iter().enumerate() {
    println!("  {}. {option}", index + 1);
  }

  let mut cursor = default;
  loop {
    let text = line("> ")?;
    if text.trim().is_empty() {
      return Some(cursor);
    }

    // A terminal in cooked mode delivers the arrow keys as their escape
    // sequence followed by Enter, so they arrive here as a line of their own.
    let moves = arrow_moves(&text);
    if !moves.is_empty() {
      for up in moves {
        cursor = step(cursor, options.len(), up);
      }
      println!("  {}. {}", cursor + 1, options[cursor]);
      continue;
    }

    match text.trim().parse::<usize>() {
      Ok(number) if (1..=options.len()).contains(&number) => return Some(number - 1),
      _ => {
        let count = options.len().to_string();
        println!("{}", app.args(Key::SetupInvalidChoice, &[&count]));
      }
    }
  }
}

fn raw(options: &[String], default: usize) -> Option<usize> {
  let mut cursor = default;
  draw(options, cursor, false);

  loop {
    match press() {
      Press::Up => {
        cursor = step(cursor, options.len(), true);
        draw(options, cursor, true);
      }
      Press::Down => {
        cursor = step(cursor, options.len(), false);
        draw(options, cursor, true);
      }
      Press::Enter => {
        erase(options.len(), Some(&options[cursor]));
        return Some(cursor);
      }
      Press::Pick(index) if index < options.len() => {
        erase(options.len(), Some(&options[index]));
        return Some(index);
      }
      Press::Cancel | Press::Eof => {
        erase(options.len(), None);
        return None;
      }
      Press::Pick(_) | Press::Other => {}
    }
  }
}

/// One press of a key, as far as the menu cares.
enum Press {
  Up,
  Down,
  Enter,
  Cancel,
  Eof,
  Pick(usize),
  Other,
}

fn press() -> Press {
  let Some(byte) = read_byte() else { return Press::Eof };
  match byte {
    b'\r' | b'\n' | b' ' => Press::Enter,
    // Ctrl-C and Ctrl-D are read as bytes here (`stty -isig` keeps the tty from
    // turning them into signals), so both mean "stop asking".
    0x03 | 0x04 | b'q' | b'Q' => Press::Cancel,
    0x1b => escape(),
    b'k' => Press::Up,
    b'j' => Press::Down,
    b'1'..=b'9' => Press::Pick(usize::from(byte - b'1')),
    _ => Press::Other,
  }
}

/// A lone ESC cannot be told apart from the start of an escape sequence
/// without waiting, so the sequence is read to its end; a key press that is
/// not an arrow is taken as a cancel, which is what ESC means by itself.
fn escape() -> Press {
  match read_byte() {
    Some(b'[' | b'O') => match read_byte() {
      Some(b'A') => Press::Up,
      Some(b'B') => Press::Down,
      _ => Press::Other,
    },
    _ => Press::Cancel,
  }
}

fn read_byte() -> Option<u8> {
  let mut byte = [0u8; 1];
  match io::stdin().lock().read(&mut byte) {
    Ok(0) | Err(_) => None,
    Ok(_) => Some(byte[0]),
  }
}

/// Draws the options under the prompt, or redraws them in place.
fn draw(options: &[String], cursor: usize, rewrite: bool) {
  if rewrite {
    print!("\x1b[{}A", options.len());
  }
  for (index, option) in options.iter().enumerate() {
    let mark = if index == cursor { "> " } else { "  " };
    print!("\r\x1b[K{mark}{option}\n");
  }
  let _ = io::stdout().flush();
}

/// Removes the option block and leaves the answer in its place, so the
/// transcript of a finished run reads like a filled-in form.
fn erase(count: usize, picked: Option<&String>) {
  print!("\x1b[{count}A\r\x1b[J");
  if let Some(label) = picked {
    println!("> {label}");
  }
  let _ = io::stdout().flush();
}

/// How many Up/Down moves a line of arrow-key escapes asks for, in order.
fn arrow_moves(text: &str) -> Vec<bool> {
  let bytes = text.as_bytes();
  let mut moves = Vec::new();
  let mut index = 0;
  while index + 2 < bytes.len() {
    if bytes[index] == 0x1b && (bytes[index + 1] == b'[' || bytes[index + 1] == b'O') {
      match bytes[index + 2] {
        b'A' => {
          moves.push(true);
          index += 3;
          continue;
        }
        b'B' => {
          moves.push(false);
          index += 3;
          continue;
        }
        _ => {}
      }
    }
    index += 1;
  }
  moves
}

fn step(cursor: usize, count: usize, up: bool) -> usize {
  if up {
    if cursor == 0 { count - 1 } else { cursor - 1 }
  } else if cursor + 1 == count {
    0
  } else {
    cursor + 1
  }
}

/// The terminal in character-at-a-time mode, restored when this is dropped.
///
/// Drop runs on a cancel, on an error and while the stack unwinds, which is
/// what keeps a failed or abandoned menu from leaving the shell without echo.
struct Terminal {
  saved: String,
}

impl Terminal {
  fn enter() -> Option<Self> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
      return None;
    }
    let saved = stty(&["-g"])?.trim().to_string();
    if saved.is_empty() {
      return None;
    }

    let guard = Self { saved };
    // One byte at a time, no echo, no signals: every key press reaches the
    // menu as a byte, including the ones the tty would otherwise interpret.
    stty(&["-icanon", "-echo", "-isig", "min", "1", "time", "0"])?;
    Some(guard)
  }
}

impl Drop for Terminal {
  fn drop(&mut self) {
    // If `stty` has disappeared there is nothing better to do than let the
    // shell reset the line on its own; the options were saved by `-g`, so a
    // restore either works completely or not at all.
    let _ = stty(&[&self.saved]);
  }
}

fn stty(args: &[&str]) -> Option<String> {
  let output =
    Command::new("stty").args(args).stdin(Stdio::inherit()).stderr(Stdio::null()).output().ok()?;
  if !output.status.success() {
    return None;
  }
  Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
  use super::{arrow_moves, step};
  use pretty_assertions::assert_eq;

  #[test]
  fn reads_arrow_keys_out_of_a_cooked_mode_line() {
    assert_eq!(arrow_moves("\x1b[B"), vec![false]);
    assert_eq!(arrow_moves("\x1b[A\x1b[A"), vec![true, true]);
    assert_eq!(arrow_moves("\x1bOB"), vec![false], "the other escape form counts too");
    assert_eq!(arrow_moves("2"), Vec::<bool>::new());
    assert_eq!(arrow_moves("\x1b[C"), Vec::<bool>::new(), "left/right are not movement");
    assert_eq!(arrow_moves("\x1b"), Vec::<bool>::new(), "a trailing escape is not a move");
  }

  #[test]
  fn the_cursor_wraps_in_both_directions() {
    assert_eq!(step(0, 3, true), 2);
    assert_eq!(step(2, 3, false), 0);
    assert_eq!(step(1, 3, true), 0);
    assert_eq!(step(1, 3, false), 2);
    assert_eq!(step(0, 1, false), 0, "a one-option menu stays put");
  }
}
