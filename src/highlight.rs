//! Syntax highlighting for code blocks, on a background thread.
//!
//! syntect compiles a language's grammar the first time it is used, which
//! takes tens of milliseconds (about 45 ms for shell) — more than everything
//! else needed to show the first screen. So `main` hands over the document's
//! code blocks as soon as the file is read, they are highlighted while the
//! rest of startup runs, and the page shows a block uncolored if its tokens
//! are not ready yet (`Doc::highlight` colors it afterwards). Results are
//! kept, so re-layouts (zoom, resize) never highlight again.

use crate::theme::Rgb;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, OnceLock};
use syntect::highlighting::{
    Color, HighlightIterator, HighlightState, Highlighter as Colorizer, ScopeSelectors, StyleModifier, Theme,
    ThemeItem, ThemeSettings,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

/// A run of code in one color.
pub struct Token {
    pub text: String,
    /// Color on a dark and on a light page; `None` = the default text color.
    /// Both are computed because the worker starts before the terminal has
    /// said which one it is.
    pub colors: [Option<Rgb>; 2],
}

pub type Tokens = Arc<Vec<Token>>;

struct Job {
    key: u64,
    generation: u64,
    lang: String,
    code: String,
}

/// What the worker did with a job.
enum Outcome {
    /// `None` for languages syntect does not know.
    Done(Option<Tokens>),
    /// The job belonged to a superseded document and was not run.
    Skipped,
}

pub struct Highlighter {
    /// Finished blocks; `None` for languages syntect does not know.
    done: HashMap<u64, Option<Tokens>>,
    /// Blocks the worker still owes an answer for.
    queued: HashSet<u64>,
    jobs: Sender<Job>,
    results: Receiver<(u64, Outcome)>,
    /// Bumped by `prepare`; the worker skips jobs of older generations.
    generation: Arc<AtomicU64>,
}

fn key(lang: &str, code: &str) -> u64 {
    let mut h = DefaultHasher::new();
    (lang, code).hash(&mut h);
    h.finish()
}

/// What "the default text color" looks like in our themes: a color no rule
/// uses, so tokens that no rule matched can be told apart.
const PLAIN: Color = Color { r: 1, g: 2, b: 3, a: 255 };

/// A small highlighting theme: a handful of colors for the kinds of token
/// that matter when reading code, everything else in the text color.
/// `colors` are comment, keyword, function, string, number, type.
fn theme(colors: [u32; 6]) -> Theme {
    let [comment, keyword, function, string, number, kind] = colors.map(|rgb| {
        let [_, r, g, b] = rgb.to_be_bytes();
        Color { r, g, b, a: 255 }
    });
    let rules = [
        ("comment, punctuation.definition.comment", comment),
        ("keyword, storage, variable.language", keyword),
        ("keyword.operator", PLAIN),
        ("keyword.operator.word, keyword.operator.new, keyword.operator.logical.python", keyword),
        ("entity.name.function, support.function, support.macro, variable.function, entity.name.tag", function),
        ("string, punctuation.definition.string", string),
        ("constant.numeric, constant.language, constant.character, support.constant", number),
        ("entity.other.attribute-name, constant.other, variable.other.constant", number),
        ("entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait", kind),
        ("support.type, support.class, entity.other.inherited-class, entity.name.namespace", kind),
        ("markup.inserted", string),
        ("markup.deleted", keyword),
        ("markup.heading, entity.name.section", function),
    ];
    Theme {
        settings: ThemeSettings { foreground: Some(PLAIN), ..ThemeSettings::default() },
        scopes: rules
            .into_iter()
            .filter_map(|(scopes, color)| {
                let style = StyleModifier { foreground: Some(color), background: None, font_style: None };
                Some(ThemeItem { scope: scopes.parse::<ScopeSelectors>().ok()?, style })
            })
            .collect(),
        ..Theme::default()
    }
}

/// The syntax definitions and our themes for a dark and a light page.
fn syntaxes() -> &'static (SyntaxSet, [Theme; 2]) {
    static LOADED: OnceLock<(SyntaxSet, [Theme; 2])> = OnceLock::new();
    LOADED.get_or_init(|| {
        let dark = theme([0x7f8694, 0xc79bf2, 0x7fb0f5, 0x93cf8d, 0x5fc9c4, 0xe2bd7a]);
        let light = theme([0x8a8f98, 0x8431c4, 0x0b5cc7, 0x17803d, 0x0a7f89, 0x9a5b00]);
        (SyntaxSet::load_defaults_newlines(), [dark, light])
    })
}

impl Highlighter {
    pub fn new() -> Highlighter {
        let (jobs, job_rx) = channel::<Job>();
        let (result_tx, results) = channel();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        std::thread::spawn(move || {
            for job in job_rx {
                // Superseded by a newer document: do not spend time on it.
                let outcome = match job.generation < current.load(Ordering::Relaxed) {
                    true => Outcome::Skipped,
                    false => Outcome::Done(highlight(&job.lang, &job.code)),
                };
                if result_tx.send((job.key, outcome)).is_err() {
                    break;
                }
            }
        });
        Highlighter { done: HashMap::new(), queued: HashSet::new(), jobs, results, generation }
    }

    /// Start highlighting a document's code blocks, given as (language,
    /// code) in document order. Blocks of earlier documents that are still
    /// waiting are dropped; a block both documents have is queued again.
    pub fn prepare(&mut self, blocks: impl IntoIterator<Item = (String, String)>) {
        self.drain();
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.queued.clear();
        for (lang, code) in blocks {
            let key = key(&lang, &code);
            if lang.is_empty() || self.done.contains_key(&key) || !self.queued.insert(key) {
                continue;
            }
            let _ = self.jobs.send(Job { key, generation, lang, code });
        }
    }

    fn receive(&mut self, (key, outcome): (u64, Outcome)) {
        if let Outcome::Done(tokens) = outcome {
            self.queued.remove(&key);
            self.done.insert(key, tokens);
        }
    }

    /// Take in whatever the worker has finished.
    fn drain(&mut self) {
        while let Ok(result) = self.results.try_recv() {
            self.receive(result);
        }
    }

    /// The highlighted tokens of a block if the worker has finished it:
    /// `Some(None)` means the language is unknown, `None` means not yet.
    pub fn try_get(&mut self, lang: &str, code: &str) -> Option<Option<Tokens>> {
        if lang.is_empty() {
            return Some(None);
        }
        self.drain();
        let key = key(lang, code);
        if !self.done.contains_key(&key) && self.queued.insert(key) {
            // Not part of the prepared document; queue it now.
            let generation = self.generation.load(Ordering::Relaxed);
            let _ = self.jobs.send(Job { key, generation, lang: lang.to_string(), code: code.to_string() });
        }
        self.done.get(&key).cloned()
    }

    /// The highlighted tokens of a block, waiting for the worker if it has
    /// not got to it yet. `None` if the language is unknown.
    pub fn get(&mut self, lang: &str, code: &str) -> Option<Tokens> {
        if lang.is_empty() {
            return None;
        }
        let key = key(lang, code);
        while !self.done.contains_key(&key) && self.queued.contains(&key) {
            match self.results.recv() {
                Ok(result) => self.receive(result),
                Err(_) => break, // worker gone; highlight here instead
            }
        }
        self.done.entry(key).or_insert_with(|| highlight(lang, code)).clone()
    }
}

fn highlight(lang: &str, code: &str) -> Option<Tokens> {
    let (set, themes) = syntaxes();
    let syntax = set.find_syntax_by_token(lang)?;
    let colorizers = themes.each_ref().map(Colorizer::new);
    let mut states = [0, 1].map(|i| HighlightState::new(&colorizers[i], ScopeStack::new()));
    let mut parser = ParseState::new(syntax);

    let mut tokens: Vec<Token> = Vec::new();
    for line in LinesWithEndings::from(code) {
        let Ok(ops) = parser.parse_line(line, set) else {
            tokens.push(Token { text: line.to_string(), colors: [None; 2] });
            continue;
        };
        // The same scope changes split the line identically for both themes.
        let [dark, light] = [0, 1].map(|i| {
            HighlightIterator::new(&mut states[i], &ops, line, &colorizers[i])
                .map(|(style, text)| {
                    // Ordinary text is drawn in the page's own text color.
                    let c = style.foreground;
                    (text, (c != PLAIN).then_some(Rgb(c.r, c.g, c.b)))
                })
                .collect::<Vec<_>>()
        });
        for ((text, dark), (_, light)) in dark.into_iter().zip(light) {
            let colors = [dark, light];
            match tokens.last_mut() {
                Some(last) if last.colors == colors => last.text.push_str(text),
                _ => tokens.push(Token { text: text.to_string(), colors }),
            }
        }
    }
    Some(Arc::new(tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(tokens: &Tokens) -> String {
        tokens.iter().map(|t| t.text.as_str()).collect()
    }

    #[test]
    fn highlights_in_background() {
        let mut h = Highlighter::new();
        let code = "fn main() {\n    let x = \"hi\";\n}";
        h.prepare([("rust".to_string(), code.to_string()), ("no-such-language".to_string(), "x".to_string())]);
        // Not ready the instant it is queued, but it arrives.
        let start = std::time::Instant::now();
        while h.try_get("rust", code).is_none() {
            assert!(start.elapsed().as_secs() < 30, "worker never answered");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let tokens = h.get("rust", code).expect("rust is a known language");
        assert_eq!(text(&tokens), code);
        // Keywords and strings are colored, in both themes.
        assert!(tokens.iter().any(|t| t.text.contains("fn") && t.colors[0].is_some() && t.colors[1].is_some()));
        assert!(h.get("no-such-language", "x").is_none());
    }

    #[test]
    fn blocks_never_prepared_are_highlighted_on_demand() {
        let mut h = Highlighter::new();
        assert_eq!(text(&h.get("sh", "echo hi").unwrap()), "echo hi");
        assert!(h.get("", "plain").is_none());
        assert!(matches!(h.try_get("", "plain"), Some(None)));
        // `try_get` queues an unknown block instead of doing the work.
        while h.try_get("sh", "echo ho").is_none() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(text(&h.try_get("sh", "echo ho").unwrap().unwrap()), "echo ho");
    }

    #[test]
    fn preparing_the_same_document_twice_keeps_every_block() {
        // What startup does: once from `main`, once when the page is built.
        let mut h = Highlighter::new();
        let blocks: Vec<(String, String)> = (0..20).map(|i| ("sh".to_string(), format!("echo {i} # note"))).collect();
        h.prepare(blocks.clone());
        h.prepare(blocks.clone());
        for (lang, code) in &blocks {
            let tokens = h.get(lang, code).expect("sh is a known language");
            assert_eq!(text(&tokens), *code);
            assert!(tokens.len() > 1, "{code:?} was not highlighted");
        }
    }

    #[test]
    fn newer_document_supersedes_queued_blocks() {
        let mut h = Highlighter::new();
        h.prepare((0..50).map(|i| ("rust".to_string(), format!("let a{i} = {i};"))));
        h.prepare([("rust".to_string(), "let z = 1;".to_string())]);
        assert_eq!(text(&h.get("rust", "let z = 1;").unwrap()), "let z = 1;");
        // A block of the old document can still be asked for.
        assert_eq!(text(&h.get("rust", "let a49 = 49;").unwrap()), "let a49 = 49;");
    }
}
