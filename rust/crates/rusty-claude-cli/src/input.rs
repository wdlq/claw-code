use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::{self, IsTerminal, Write};

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, CompletionType, Config, Context, EditMode, Editor, Helper, KeyCode, KeyEvent, Modifiers,
};

const PASTE_LINE_THRESHOLD: usize = 2;
const PASTE_DISPLAY_THRESHOLD: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    Submit(String),
    Cancel,
    Exit,
}

/// Manages paste detection, counting, and content storage.
pub struct PasteManager {
    paste_count: usize,
    pastes: HashMap<String, String>,
    lines_to_consume: usize,
    pending_label: Option<String>,
    is_large_paste: bool,
}

impl PasteManager {
    pub fn new() -> Self {
        Self {
            paste_count: 0,
            pastes: HashMap::new(),
            lines_to_consume: 0,
            pending_label: None,
            is_large_paste: false,
        }
    }

    /// Register a paste and return its label if it exceeds the threshold.
    pub fn register_paste(&mut self, content: &str) -> Option<String> {
        let line_count = content.lines().count();
        if line_count <= PASTE_LINE_THRESHOLD {
            return None;
        }

        self.paste_count += 1;
        let label = format!("[Paste text #{} +{} lines]", self.paste_count, line_count);
        self.pastes.insert(label.clone(), content.to_string());
        Some(label)
    }

    /// Resolve all paste labels in the input to their full content.
    pub fn resolve_all_labels(&self, input: &str) -> String {
        let mut result = input.to_string();
        for (label, content) in &self.pastes {
            result = result.replace(label, content);
        }
        result
    }

    /// Check if input contains any paste labels.
    pub fn has_labels(&self, input: &str) -> bool {
        self.pastes.keys().any(|label| input.contains(label))
    }
}

struct SlashCommandHelper {
    completions: Vec<String>,
    current_line: RefCell<String>,
}

impl SlashCommandHelper {
    fn new(completions: Vec<String>) -> Self {
        Self {
            completions: normalize_completions(completions),
            current_line: RefCell::new(String::new()),
        }
    }

    fn reset_current_line(&self) {
        self.current_line.borrow_mut().clear();
    }

    fn current_line(&self) -> String {
        self.current_line.borrow().clone()
    }

    fn set_current_line(&self, line: &str) {
        let mut current = self.current_line.borrow_mut();
        current.clear();
        current.push_str(line);
    }

    fn set_completions(&mut self, completions: Vec<String>) {
        self.completions = normalize_completions(completions);
    }
}

impl Completer for SlashCommandHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        let Some(prefix) = slash_command_prefix(line, pos) else {
            return Ok((0, Vec::new()));
        };

        let matches = self
            .completions
            .iter()
            .filter(|candidate| candidate.starts_with(prefix))
            .map(|candidate| Pair {
                display: candidate.clone(),
                replacement: candidate.clone(),
            })
            .collect();

        Ok((0, matches))
    }
}

impl Hinter for SlashCommandHelper {
    type Hint = String;
}

impl Highlighter for SlashCommandHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        self.set_current_line(line);
        Cow::Borrowed(line)
    }

    fn highlight_char(&self, line: &str, _pos: usize, _kind: CmdKind) -> bool {
        self.set_current_line(line);
        false
    }
}

impl Validator for SlashCommandHelper {}
impl Helper for SlashCommandHelper {}

pub struct LineEditor {
    prompt: String,
    editor: Editor<SlashCommandHelper, DefaultHistory>,
    paste_manager: PasteManager,
}

impl LineEditor {
    #[must_use]
    pub fn new(prompt: impl Into<String>, completions: Vec<String>) -> Self {
        let config = Config::builder()
            .completion_type(CompletionType::List)
            .edit_mode(EditMode::Emacs)
            .bracketed_paste(true)
            .build();
        let mut editor = Editor::<SlashCommandHelper, DefaultHistory>::with_config(config)
            .expect("rustyline editor should initialize");
        editor.set_helper(Some(SlashCommandHelper::new(completions)));
        editor.bind_sequence(KeyEvent(KeyCode::Char('J'), Modifiers::CTRL), Cmd::Newline);
        editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::SHIFT), Cmd::Newline);

        Self {
            prompt: prompt.into(),
            editor,
            paste_manager: PasteManager::new(),
        }
    }

    /// Get a reference to the paste manager.
    pub fn paste_manager(&self) -> &PasteManager {
        &self.paste_manager
    }

    /// Get a mutable reference to the paste manager.
    pub fn paste_manager_mut(&mut self) -> &mut PasteManager {
        &mut self.paste_manager
    }

    pub fn push_history(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        if entry.trim().is_empty() {
            return;
        }

        let _ = self.editor.add_history_entry(entry);
    }

    pub fn set_completions(&mut self, completions: Vec<String>) {
        if let Some(helper) = self.editor.helper_mut() {
            helper.set_completions(completions);
        }
    }

    pub fn read_line(&mut self) -> io::Result<ReadOutcome> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return self.read_line_fallback();
        }

        if let Some(helper) = self.editor.helper_mut() {
            helper.reset_current_line();
        }

        match self.editor.readline(&self.prompt) {
            Ok(line) => {
                // Check if this is a multi-line paste that exceeds threshold
                let line_count = line.lines().count();
                if line_count > PASTE_LINE_THRESHOLD {
                    // Register the paste and get label
                    if let Some(label) = self.paste_manager.register_paste(&line) {
                        // Print the label on a new line
                        let mut stdout = io::stdout();
                        writeln!(stdout)?;
                        writeln!(stdout, "{}", label)?;
                        write!(stdout, "{}", self.prompt)?;
                        stdout.flush()?;

                        // Return the label as the input (will be resolved later)
                        return Ok(ReadOutcome::Submit(label));
                    }
                }
                Ok(ReadOutcome::Submit(line))
            }
            Err(ReadlineError::Interrupted) => {
                let has_input = !self.current_line().is_empty();
                self.finish_interrupted_read()?;
                if has_input {
                    Ok(ReadOutcome::Cancel)
                } else {
                    Ok(ReadOutcome::Exit)
                }
            }
            Err(ReadlineError::Eof) => {
                self.finish_interrupted_read()?;
                Ok(ReadOutcome::Exit)
            }
            Err(error) => Err(io::Error::other(error)),
        }
    }

    /// Read a line with paste detection.
    /// After readline returns, check if the result looks like a paste
    /// from the clipboard. If so, register it and return the label.
    /// On subsequent calls, skip lines that are part of the paste injection.
    pub fn read_line_with_paste_detection(&mut self) -> io::Result<ReadOutcome> {
        // If we have lines to skip (from a previous paste detection),
        // call readline to consume the injected line, but don't submit it
        if self.paste_manager.lines_to_consume > 0 {
            self.paste_manager.lines_to_consume -= 1;
            if let Some(helper) = self.editor.helper_mut() {
                helper.reset_current_line();
            }
            // Read the injected line (this consumes it from the console buffer)
            match self.editor.readline(&self.prompt) {
                Ok(_) => {
                    // For large pastes, clear the consumed line from terminal
                    if self.paste_manager.is_large_paste {
                        let mut stdout = io::stdout();
                        // Move cursor up 1 line and clear it
                        write!(stdout, "\x1b[1A\x1b[2K")?;
                        stdout.flush()?;
                    }

                    // If this was the last line to skip, let user type more
                    if self.paste_manager.lines_to_consume == 0 {
                        if let Some(label) = self.paste_manager.pending_label.take() {
                            self.paste_manager.is_large_paste = false;

                            // Call read_line_with_paste_detection for additional input
                            // This allows detecting a second paste
                            match self.read_line_with_paste_detection() {
                                Ok(ReadOutcome::Submit(additional)) => {
                                    let additional = additional.trim();
                                    if additional.is_empty() {
                                        return Ok(ReadOutcome::Submit(label));
                                    } else {
                                        let combined = format!("{}\n{}", label, additional);
                                        return Ok(ReadOutcome::Submit(combined));
                                    }
                                }
                                Ok(outcome) => return Ok(outcome),
                                Err(error) => return Err(error),
                            }
                        }
                    }
                    // More lines to skip, recurse
                    return self.read_line_with_paste_detection();
                }
                Err(ReadlineError::Interrupted) => {
                    self.finish_interrupted_read()?;
                    self.paste_manager.lines_to_consume = 0;
                    if let Some(label) = self.paste_manager.pending_label.take() {
                        return Ok(ReadOutcome::Submit(label));
                    }
                    return Ok(ReadOutcome::Cancel);
                }
                Err(ReadlineError::Eof) => {
                    self.finish_interrupted_read()?;
                    self.paste_manager.lines_to_consume = 0;
                    self.paste_manager.pending_label = None;
                    return Ok(ReadOutcome::Exit);
                }
                Err(error) => {
                    self.paste_manager.lines_to_consume = 0;
                    self.paste_manager.pending_label = None;
                    return Err(io::Error::other(error));
                }
            }
        }

        // Save clipboard before readline for comparison
        let clipboard_before = clipboard_win::get_clipboard_string().unwrap_or_default();

        if let Some(helper) = self.editor.helper_mut() {
            helper.reset_current_line();
        }

        match self.editor.readline(&self.prompt) {
            Ok(line) => {
                // Read clipboard AFTER readline returns
                let clipboard_after = clipboard_win::get_clipboard_string().unwrap_or_default();

                // Check if clipboard changed and looks like a paste
                let clipboard_changed = clipboard_before != clipboard_after;
                let clipboard_line_count = clipboard_after.lines().count();

                if clipboard_line_count > PASTE_LINE_THRESHOLD {
                    let first_line = clipboard_after.lines().next().unwrap_or("");
                    // If the returned line ends with the first line of clipboard,
                    // it's likely a paste (user may have typed text before pasting)
                    if line.trim().ends_with(first_line.trim()) && !first_line.trim().is_empty() {
                        if let Some(label) =
                            self.paste_manager.register_paste(clipboard_after.trim())
                        {
                            // Check if this is a large paste that should suppress display
                            self.paste_manager.is_large_paste =
                                clipboard_line_count > PASTE_DISPLAY_THRESHOLD;

                            let mut stdout = io::stdout();
                            writeln!(stdout)?;
                            writeln!(stdout, "{}", label)?;
                            stdout.flush()?;

                            // Set up to skip remaining paste lines
                            // The remaining lines (2..N) are already in the console buffer
                            self.paste_manager.lines_to_consume = clipboard_line_count - 1;
                            self.paste_manager.pending_label = Some(label.clone());

                            // Start skipping remaining lines
                            return self.read_line_with_paste_detection();
                        }
                    }
                }

                // Also check the returned line itself for multi-line content
                let line_count = line.lines().count();
                if line_count > PASTE_LINE_THRESHOLD {
                    if let Some(label) = self.paste_manager.register_paste(&line) {
                        let mut stdout = io::stdout();
                        writeln!(stdout)?;
                        writeln!(stdout, "{}", label)?;
                        stdout.flush()?;
                        return Ok(ReadOutcome::Submit(label));
                    }
                }

                Ok(ReadOutcome::Submit(line))
            }
            Err(ReadlineError::Interrupted) => {
                let has_input = !self.current_line().is_empty();
                self.finish_interrupted_read()?;
                if has_input {
                    Ok(ReadOutcome::Cancel)
                } else {
                    Ok(ReadOutcome::Exit)
                }
            }
            Err(ReadlineError::Eof) => {
                self.finish_interrupted_read()?;
                Ok(ReadOutcome::Exit)
            }
            Err(error) => Err(io::Error::other(error)),
        }
    }

    fn current_line(&self) -> String {
        self.editor
            .helper()
            .map_or_else(String::new, SlashCommandHelper::current_line)
    }

    fn finish_interrupted_read(&mut self) -> io::Result<()> {
        if let Some(helper) = self.editor.helper_mut() {
            helper.reset_current_line();
        }
        let mut stdout = io::stdout();
        writeln!(stdout)
    }

    fn read_line_fallback(&self) -> io::Result<ReadOutcome> {
        let mut stdout = io::stdout();
        write!(stdout, "{}", self.prompt)?;
        stdout.flush()?;

        let mut buffer = String::new();
        let bytes_read = io::stdin().read_line(&mut buffer)?;
        if bytes_read == 0 {
            return Ok(ReadOutcome::Exit);
        }

        while matches!(buffer.chars().last(), Some('\n' | '\r')) {
            buffer.pop();
        }
        Ok(ReadOutcome::Submit(buffer))
    }
}

fn slash_command_prefix(line: &str, pos: usize) -> Option<&str> {
    if pos != line.len() {
        return None;
    }

    let prefix = &line[..pos];
    if !prefix.starts_with('/') {
        return None;
    }

    Some(prefix)
}

fn normalize_completions(completions: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    completions
        .into_iter()
        .filter(|candidate| candidate.starts_with('/'))
        .filter(|candidate| seen.insert(candidate.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{slash_command_prefix, LineEditor, SlashCommandHelper};
    use rustyline::completion::Completer;
    use rustyline::highlight::Highlighter;
    use rustyline::history::{DefaultHistory, History};
    use rustyline::Context;

    #[test]
    fn extracts_terminal_slash_command_prefixes_with_arguments() {
        assert_eq!(slash_command_prefix("/he", 3), Some("/he"));
        assert_eq!(slash_command_prefix("/help me", 8), Some("/help me"));
        assert_eq!(
            slash_command_prefix("/session switch ses", 19),
            Some("/session switch ses")
        );
        assert_eq!(slash_command_prefix("hello", 5), None);
        assert_eq!(slash_command_prefix("/help", 2), None);
    }

    #[test]
    fn completes_matching_slash_commands() {
        let helper = SlashCommandHelper::new(vec![
            "/help".to_string(),
            "/hello".to_string(),
            "/status".to_string(),
        ]);
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (start, matches) = helper
            .complete("/he", 3, &ctx)
            .expect("completion should work");

        assert_eq!(start, 0);
        assert_eq!(
            matches
                .into_iter()
                .map(|candidate| candidate.replacement)
                .collect::<Vec<_>>(),
            vec!["/help".to_string(), "/hello".to_string()]
        );
    }

    #[test]
    fn completes_matching_slash_command_arguments() {
        let helper = SlashCommandHelper::new(vec![
            "/model".to_string(),
            "/model opus".to_string(),
            "/model sonnet".to_string(),
            "/session switch alpha".to_string(),
        ]);
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (start, matches) = helper
            .complete("/model o", 8, &ctx)
            .expect("completion should work");

        assert_eq!(start, 0);
        assert_eq!(
            matches
                .into_iter()
                .map(|candidate| candidate.replacement)
                .collect::<Vec<_>>(),
            vec!["/model opus".to_string()]
        );
    }

    #[test]
    fn ignores_non_slash_command_completion_requests() {
        let helper = SlashCommandHelper::new(vec!["/help".to_string()]);
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (_, matches) = helper
            .complete("hello", 5, &ctx)
            .expect("completion should work");

        assert!(matches.is_empty());
    }

    #[test]
    fn tracks_current_buffer_through_highlighter() {
        let helper = SlashCommandHelper::new(Vec::new());
        let _ = helper.highlight("draft", 5);

        assert_eq!(helper.current_line(), "draft");
    }

    #[test]
    fn push_history_ignores_blank_entries() {
        let mut editor = LineEditor::new("> ", vec!["/help".to_string()]);
        editor.push_history("   ");
        editor.push_history("/help");

        assert_eq!(editor.editor.history().len(), 1);
    }

    #[test]
    fn set_completions_replaces_and_normalizes_candidates() {
        let mut editor = LineEditor::new("> ", vec!["/help".to_string()]);
        editor.set_completions(vec![
            "/model opus".to_string(),
            "/model opus".to_string(),
            "status".to_string(),
        ]);

        let helper = editor.editor.helper().expect("helper should exist");
        assert_eq!(helper.completions, vec!["/model opus".to_string()]);
    }
}
