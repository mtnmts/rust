//! Diagnostics creation and emission for `rustc`.
//!
//! This module contains the code for creating and emitting diagnostics.

#![doc(html_root_url = "https://doc.rust-lang.org/nightly/")]

#![feature(crate_visibility_modifier)]
#![cfg_attr(unix, feature(libc))]
#![feature(nll)]
#![feature(optin_builtin_traits)]

pub use emitter::ColorConfig;

use Level::*;

use emitter::{Emitter, EmitterWriter};
use registry::Registry;

use rustc_data_structures::sync::{self, Lrc, Lock};
use rustc_data_structures::fx::FxHashSet;
use rustc_data_structures::stable_hasher::StableHasher;

use std::borrow::Cow;
use std::cell::Cell;
use std::{error, fmt};
use std::panic;
use std::path::Path;

use termcolor::{ColorSpec, Color};

mod diagnostic;
mod diagnostic_builder;
pub mod emitter;
pub mod annotate_snippet_emitter_writer;
mod snippet;
pub mod registry;
mod styled_buffer;
mod lock;

use syntax_pos::{BytePos,
                 Loc,
                 FileLinesResult,
                 SourceFile,
                 FileName,
                 MultiSpan,
                 Span};

/// Indicates the confidence in the correctness of a suggestion.
///
/// All suggestions are marked with an `Applicability`. Tools use the applicability of a suggestion
/// to determine whether it should be automatically applied or if the user should be consulted
/// before applying the suggestion.
#[derive(Copy, Clone, Debug, PartialEq, Hash, RustcEncodable, RustcDecodable)]
pub enum Applicability {
    /// The suggestion is definitely what the user intended. This suggestion should be
    /// automatically applied.
    MachineApplicable,

    /// The suggestion may be what the user intended, but it is uncertain. The suggestion should
    /// result in valid Rust code if it is applied.
    MaybeIncorrect,

    /// The suggestion contains placeholders like `(...)` or `{ /* fields */ }`. The suggestion
    /// cannot be applied automatically because it will not result in valid Rust code. The user
    /// will need to fill in the placeholders.
    HasPlaceholders,

    /// The applicability of the suggestion is unknown.
    Unspecified,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash, RustcEncodable, RustcDecodable)]
pub enum SuggestionStyle {
    /// Hide the suggested code when displaying this suggestion inline.
    HideCodeInline,
    /// Always hide the suggested code but display the message.
    HideCodeAlways,
    /// Do not display this suggestion in the cli output, it is only meant for tools.
    CompletelyHidden,
    /// Always show the suggested code.
    /// This will *not* show the code if the suggestion is inline *and* the suggested code is
    /// empty.
    ShowCode,
}

impl SuggestionStyle {
    fn hide_inline(&self) -> bool {
        match *self {
            SuggestionStyle::ShowCode => false,
            _ => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Hash, RustcEncodable, RustcDecodable)]
pub struct CodeSuggestion {
    /// Each substitute can have multiple variants due to multiple
    /// applicable suggestions
    ///
    /// `foo.bar` might be replaced with `a.b` or `x.y` by replacing
    /// `foo` and `bar` on their own:
    ///
    /// ```
    /// vec![
    ///     Substitution { parts: vec![(0..3, "a"), (4..7, "b")] },
    ///     Substitution { parts: vec![(0..3, "x"), (4..7, "y")] },
    /// ]
    /// ```
    ///
    /// or by replacing the entire span:
    ///
    /// ```
    /// vec![
    ///     Substitution { parts: vec![(0..7, "a.b")] },
    ///     Substitution { parts: vec![(0..7, "x.y")] },
    /// ]
    /// ```
    pub substitutions: Vec<Substitution>,
    pub msg: String,
    /// Visual representation of this suggestion.
    pub style: SuggestionStyle,
    /// Whether or not the suggestion is approximate
    ///
    /// Sometimes we may show suggestions with placeholders,
    /// which are useful for users but not useful for
    /// tools like rustfix
    pub applicability: Applicability,
}

#[derive(Clone, Debug, PartialEq, Hash, RustcEncodable, RustcDecodable)]
/// See the docs on `CodeSuggestion::substitutions`
pub struct Substitution {
    pub parts: Vec<SubstitutionPart>,
}

#[derive(Clone, Debug, PartialEq, Hash, RustcEncodable, RustcDecodable)]
pub struct SubstitutionPart {
    pub span: Span,
    pub snippet: String,
}

pub type SourceMapperDyn = dyn SourceMapper + sync::Send + sync::Sync;

pub trait SourceMapper {
    fn lookup_char_pos(&self, pos: BytePos) -> Loc;
    fn span_to_lines(&self, sp: Span) -> FileLinesResult;
    fn span_to_string(&self, sp: Span) -> String;
    fn span_to_filename(&self, sp: Span) -> FileName;
    fn merge_spans(&self, sp_lhs: Span, sp_rhs: Span) -> Option<Span>;
    fn call_span_if_macro(&self, sp: Span) -> Span;
    fn ensure_source_file_source_present(&self, source_file: Lrc<SourceFile>) -> bool;
    fn doctest_offset_line(&self, file: &FileName, line: usize) -> usize;
}

impl CodeSuggestion {
    /// Returns the assembled code suggestions and whether they should be shown with an underline.
    pub fn splice_lines(&self, cm: &SourceMapperDyn)
                        -> Vec<(String, Vec<SubstitutionPart>)> {
        use syntax_pos::{CharPos, Pos};

        fn push_trailing(buf: &mut String,
                         line_opt: Option<&Cow<'_, str>>,
                         lo: &Loc,
                         hi_opt: Option<&Loc>) {
            let (lo, hi_opt) = (lo.col.to_usize(), hi_opt.map(|hi| hi.col.to_usize()));
            if let Some(line) = line_opt {
                if let Some(lo) = line.char_indices().map(|(i, _)| i).nth(lo) {
                    let hi_opt = hi_opt.and_then(|hi| line.char_indices().map(|(i, _)| i).nth(hi));
                    match hi_opt {
                        Some(hi) if hi > lo => buf.push_str(&line[lo..hi]),
                        Some(_) => (),
                        None => buf.push_str(&line[lo..]),
                    }
                }
                if let None = hi_opt {
                    buf.push('\n');
                }
            }
        }

        assert!(!self.substitutions.is_empty());

        self.substitutions.iter().cloned().map(|mut substitution| {
            // Assumption: all spans are in the same file, and all spans
            // are disjoint. Sort in ascending order.
            substitution.parts.sort_by_key(|part| part.span.lo());

            // Find the bounding span.
            let lo = substitution.parts.iter().map(|part| part.span.lo()).min().unwrap();
            let hi = substitution.parts.iter().map(|part| part.span.hi()).min().unwrap();
            let bounding_span = Span::with_root_ctxt(lo, hi);
            let lines = cm.span_to_lines(bounding_span).unwrap();
            assert!(!lines.lines.is_empty());

            // To build up the result, we do this for each span:
            // - push the line segment trailing the previous span
            //   (at the beginning a "phantom" span pointing at the start of the line)
            // - push lines between the previous and current span (if any)
            // - if the previous and current span are not on the same line
            //   push the line segment leading up to the current span
            // - splice in the span substitution
            //
            // Finally push the trailing line segment of the last span
            let fm = &lines.file;
            let mut prev_hi = cm.lookup_char_pos(bounding_span.lo());
            prev_hi.col = CharPos::from_usize(0);

            let mut prev_line = fm.get_line(lines.lines[0].line_index);
            let mut buf = String::new();

            for part in &substitution.parts {
                let cur_lo = cm.lookup_char_pos(part.span.lo());
                if prev_hi.line == cur_lo.line {
                    push_trailing(&mut buf, prev_line.as_ref(), &prev_hi, Some(&cur_lo));
                } else {
                    push_trailing(&mut buf, prev_line.as_ref(), &prev_hi, None);
                    // push lines between the previous and current span (if any)
                    for idx in prev_hi.line..(cur_lo.line - 1) {
                        if let Some(line) = fm.get_line(idx) {
                            buf.push_str(line.as_ref());
                            buf.push('\n');
                        }
                    }
                    if let Some(cur_line) = fm.get_line(cur_lo.line - 1) {
                        let end = std::cmp::min(cur_line.len(), cur_lo.col.to_usize());
                        buf.push_str(&cur_line[..end]);
                    }
                }
                buf.push_str(&part.snippet);
                prev_hi = cm.lookup_char_pos(part.span.hi());
                prev_line = fm.get_line(prev_hi.line - 1);
            }
            // if the replacement already ends with a newline, don't print the next line
            if !buf.ends_with('\n') {
                push_trailing(&mut buf, prev_line.as_ref(), &prev_hi, None);
            }
            // remove trailing newlines
            while buf.ends_with('\n') {
                buf.pop();
            }
            (buf, substitution.parts)
        }).collect()
    }
}

/// Used as a return value to signify a fatal error occurred. (It is also
/// used as the argument to panic at the moment, but that will eventually
/// not be true.)
#[derive(Copy, Clone, Debug)]
#[must_use]
pub struct FatalError;

pub struct FatalErrorMarker;

// Don't implement Send on FatalError. This makes it impossible to panic!(FatalError).
// We don't want to invoke the panic handler and print a backtrace for fatal errors.
impl !Send for FatalError {}

impl FatalError {
    pub fn raise(self) -> ! {
        panic::resume_unwind(Box::new(FatalErrorMarker))
    }
}

impl fmt::Display for FatalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parser fatal error")
    }
}

impl error::Error for FatalError {
    fn description(&self) -> &str {
        "The parser has encountered a fatal error"
    }
}

/// Signifies that the compiler died with an explicit call to `.bug`
/// or `.span_bug` rather than a failed assertion, etc.
#[derive(Copy, Clone, Debug)]
pub struct ExplicitBug;

impl fmt::Display for ExplicitBug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parser internal bug")
    }
}

impl error::Error for ExplicitBug {
    fn description(&self) -> &str {
        "The parser has encountered an internal bug"
    }
}

pub use diagnostic::{Diagnostic, SubDiagnostic, DiagnosticStyledString, DiagnosticId};
pub use diagnostic_builder::DiagnosticBuilder;

/// A handler deals with errors and other compiler output.
/// Certain errors (fatal, bug, unimpl) may cause immediate exit,
/// others log errors for later reporting.
pub struct Handler {
    flags: HandlerFlags,
    inner: Lock<HandlerInner>,
}

struct HandlerInner {
    flags: HandlerFlags,
    /// The number of errors that have been emitted, including duplicates.
    ///
    /// This is not necessarily the count that's reported to the user once
    /// compilation ends.
    err_count: usize,
    deduplicated_err_count: usize,
    emitter: Box<dyn Emitter + sync::Send>,
    continue_after_error: bool,
    delayed_span_bugs: Vec<Diagnostic>,

    /// This set contains the `DiagnosticId` of all emitted diagnostics to avoid
    /// emitting the same diagnostic with extended help (`--teach`) twice, which
    /// would be uneccessary repetition.
    taught_diagnostics: FxHashSet<DiagnosticId>,

    /// Used to suggest rustc --explain <error code>
    emitted_diagnostic_codes: FxHashSet<DiagnosticId>,

    /// This set contains a hash of every diagnostic that has been emitted by
    /// this handler. These hashes is used to avoid emitting the same error
    /// twice.
    emitted_diagnostics: FxHashSet<u128>,
}

fn default_track_diagnostic(_: &Diagnostic) {}

thread_local!(pub static TRACK_DIAGNOSTICS: Cell<fn(&Diagnostic)> =
                Cell::new(default_track_diagnostic));

#[derive(Copy, Clone, Default)]
pub struct HandlerFlags {
    /// If false, warning-level lints are suppressed.
    /// (rustc: see `--allow warnings` and `--cap-lints`)
    pub can_emit_warnings: bool,
    /// If true, error-level diagnostics are upgraded to bug-level.
    /// (rustc: see `-Z treat-err-as-bug`)
    pub treat_err_as_bug: Option<usize>,
    /// If true, immediately emit diagnostics that would otherwise be buffered.
    /// (rustc: see `-Z dont-buffer-diagnostics` and `-Z treat-err-as-bug`)
    pub dont_buffer_diagnostics: bool,
    /// If true, immediately print bugs registered with `delay_span_bug`.
    /// (rustc: see `-Z report-delayed-bugs`)
    pub report_delayed_bugs: bool,
    /// show macro backtraces even for non-local macros.
    /// (rustc: see `-Z external-macro-backtrace`)
    pub external_macro_backtrace: bool,
}

impl Drop for HandlerInner {
    fn drop(&mut self) {
        if self.err_count == 0 {
            let bugs = std::mem::replace(&mut self.delayed_span_bugs, Vec::new());
            let has_bugs = !bugs.is_empty();
            for bug in bugs {
                self.emit_diagnostic(&bug);
            }
            if has_bugs {
                panic!("no errors encountered even though `delay_span_bug` issued");
            }
        }
    }
}

impl Handler {
    pub fn with_tty_emitter(color_config: ColorConfig,
                            can_emit_warnings: bool,
                            treat_err_as_bug: Option<usize>,
                            cm: Option<Lrc<SourceMapperDyn>>)
                            -> Handler {
        Handler::with_tty_emitter_and_flags(
            color_config,
            cm,
            HandlerFlags {
                can_emit_warnings,
                treat_err_as_bug,
                .. Default::default()
            })
    }

    pub fn with_tty_emitter_and_flags(color_config: ColorConfig,
                                      cm: Option<Lrc<SourceMapperDyn>>,
                                      flags: HandlerFlags)
                                      -> Handler {
        let emitter = Box::new(EmitterWriter::stderr(
            color_config, cm, false, false, None, flags.external_macro_backtrace));
        Handler::with_emitter_and_flags(emitter, flags)
    }

    pub fn with_emitter(can_emit_warnings: bool,
                        treat_err_as_bug: Option<usize>,
                        e: Box<dyn Emitter + sync::Send>)
                        -> Handler {
        Handler::with_emitter_and_flags(
            e,
            HandlerFlags {
                can_emit_warnings,
                treat_err_as_bug,
                .. Default::default()
            })
    }

    pub fn with_emitter_and_flags(e: Box<dyn Emitter + sync::Send>, flags: HandlerFlags) -> Handler
    {
        Handler {
            flags,
            inner: Lock::new(HandlerInner {
                flags,
                err_count: 0,
                deduplicated_err_count: 0,
                emitter: e,
                continue_after_error: true,
                delayed_span_bugs: Vec::new(),
                taught_diagnostics: Default::default(),
                emitted_diagnostic_codes: Default::default(),
                emitted_diagnostics: Default::default(),
            }),
        }
    }

    pub fn set_continue_after_error(&self, continue_after_error: bool) {
        self.inner.borrow_mut().continue_after_error = continue_after_error;
    }

    // This is here to not allow mutation of flags;
    // as of this writing it's only used in tests in librustc.
    pub fn can_emit_warnings(&self) -> bool {
        self.flags.can_emit_warnings
    }

    /// Resets the diagnostic error count as well as the cached emitted diagnostics.
    ///
    /// NOTE: *do not* call this function from rustc. It is only meant to be called from external
    /// tools that want to reuse a `Parser` cleaning the previously emitted diagnostics as well as
    /// the overall count of emitted error diagnostics.
    // FIXME: this does not clear inner entirely
    pub fn reset_err_count(&self) {
        let mut inner = self.inner.borrow_mut();
        // actually frees the underlying memory (which `clear` would not do)
        inner.emitted_diagnostics = Default::default();
        inner.deduplicated_err_count = 0;
        inner.err_count = 0;
    }

    pub fn struct_dummy(&self) -> DiagnosticBuilder<'_> {
        DiagnosticBuilder::new(self, Level::Cancelled, "")
    }

    pub fn struct_span_warn<S: Into<MultiSpan>>(&self,
                                                sp: S,
                                                msg: &str)
                                                -> DiagnosticBuilder<'_> {
        let mut result = DiagnosticBuilder::new(self, Level::Warning, msg);
        result.set_span(sp);
        if !self.flags.can_emit_warnings {
            result.cancel();
        }
        result
    }
    pub fn struct_span_warn_with_code<S: Into<MultiSpan>>(&self,
                                                          sp: S,
                                                          msg: &str,
                                                          code: DiagnosticId)
                                                          -> DiagnosticBuilder<'_> {
        let mut result = DiagnosticBuilder::new(self, Level::Warning, msg);
        result.set_span(sp);
        result.code(code);
        if !self.flags.can_emit_warnings {
            result.cancel();
        }
        result
    }
    pub fn struct_warn(&self, msg: &str) -> DiagnosticBuilder<'_> {
        let mut result = DiagnosticBuilder::new(self, Level::Warning, msg);
        if !self.flags.can_emit_warnings {
            result.cancel();
        }
        result
    }
    pub fn struct_span_err<S: Into<MultiSpan>>(&self,
                                               sp: S,
                                               msg: &str)
                                               -> DiagnosticBuilder<'_> {
        let mut result = DiagnosticBuilder::new(self, Level::Error, msg);
        result.set_span(sp);
        result
    }
    pub fn struct_span_err_with_code<S: Into<MultiSpan>>(&self,
                                                         sp: S,
                                                         msg: &str,
                                                         code: DiagnosticId)
                                                         -> DiagnosticBuilder<'_> {
        let mut result = DiagnosticBuilder::new(self, Level::Error, msg);
        result.set_span(sp);
        result.code(code);
        result
    }
    // FIXME: This method should be removed (every error should have an associated error code).
    pub fn struct_err(&self, msg: &str) -> DiagnosticBuilder<'_> {
        DiagnosticBuilder::new(self, Level::Error, msg)
    }
    pub fn struct_err_with_code(
        &self,
        msg: &str,
        code: DiagnosticId,
    ) -> DiagnosticBuilder<'_> {
        let mut result = DiagnosticBuilder::new(self, Level::Error, msg);
        result.code(code);
        result
    }
    pub fn struct_span_fatal<S: Into<MultiSpan>>(&self,
                                                 sp: S,
                                                 msg: &str)
                                                 -> DiagnosticBuilder<'_> {
        let mut result = DiagnosticBuilder::new(self, Level::Fatal, msg);
        result.set_span(sp);
        result
    }
    pub fn struct_span_fatal_with_code<S: Into<MultiSpan>>(&self,
                                                           sp: S,
                                                           msg: &str,
                                                           code: DiagnosticId)
                                                           -> DiagnosticBuilder<'_> {
        let mut result = DiagnosticBuilder::new(self, Level::Fatal, msg);
        result.set_span(sp);
        result.code(code);
        result
    }
    pub fn struct_fatal(&self, msg: &str) -> DiagnosticBuilder<'_> {
        DiagnosticBuilder::new(self, Level::Fatal, msg)
    }

    pub fn span_fatal<S: Into<MultiSpan>>(&self, sp: S, msg: &str) -> FatalError {
        self.emit_diagnostic(Diagnostic::new(Fatal, msg).set_span(sp));
        self.abort_if_errors_and_should_abort();
        FatalError
    }
    pub fn span_fatal_with_code<S: Into<MultiSpan>>(&self,
                                                    sp: S,
                                                    msg: &str,
                                                    code: DiagnosticId)
                                                    -> FatalError {
        self.emit_diagnostic(Diagnostic::new_with_code(Fatal, Some(code), msg).set_span(sp));
        self.abort_if_errors_and_should_abort();
        FatalError
    }
    pub fn span_err<S: Into<MultiSpan>>(&self, sp: S, msg: &str) {
        self.emit_diagnostic(Diagnostic::new(Error, msg).set_span(sp));
        self.abort_if_errors_and_should_abort();
    }
    pub fn mut_span_err<S: Into<MultiSpan>>(&self,
                                            sp: S,
                                            msg: &str)
                                            -> DiagnosticBuilder<'_> {
        let mut result = DiagnosticBuilder::new(self, Level::Error, msg);
        result.set_span(sp);
        result
    }
    pub fn span_err_with_code<S: Into<MultiSpan>>(&self, sp: S, msg: &str, code: DiagnosticId) {
        self.emit_diagnostic(Diagnostic::new_with_code(Error, Some(code), msg).set_span(sp));
        self.abort_if_errors_and_should_abort();
    }
    pub fn span_warn<S: Into<MultiSpan>>(&self, sp: S, msg: &str) {
        self.emit_diagnostic(Diagnostic::new(Warning, msg).set_span(sp));
        self.abort_if_errors_and_should_abort();
    }
    pub fn span_warn_with_code<S: Into<MultiSpan>>(&self, sp: S, msg: &str, code: DiagnosticId) {
        self.emit_diagnostic(Diagnostic::new_with_code(Warning, Some(code), msg).set_span(sp));
        self.abort_if_errors_and_should_abort();
    }
    pub fn span_bug<S: Into<MultiSpan>>(&self, sp: S, msg: &str) -> ! {
        self.inner.borrow_mut().span_bug(sp, msg)
    }
    pub fn delay_span_bug<S: Into<MultiSpan>>(&self, sp: S, msg: &str) {
        self.inner.borrow_mut().delay_span_bug(sp, msg)
    }
    pub fn span_bug_no_panic<S: Into<MultiSpan>>(&self, sp: S, msg: &str) {
        self.emit_diagnostic(Diagnostic::new(Bug, msg).set_span(sp));
        self.abort_if_errors_and_should_abort();
    }
    pub fn span_note_without_error<S: Into<MultiSpan>>(&self, sp: S, msg: &str) {
        self.emit_diagnostic(Diagnostic::new(Note, msg).set_span(sp));
        self.abort_if_errors_and_should_abort();
    }
    pub fn span_note_diag(&self,
                          sp: Span,
                          msg: &str)
                          -> DiagnosticBuilder<'_> {
        let mut db = DiagnosticBuilder::new(self, Note, msg);
        db.set_span(sp);
        db
    }
    pub fn failure(&self, msg: &str) {
        self.inner.borrow_mut().failure(msg);
    }
    pub fn fatal(&self, msg: &str) -> FatalError {
        self.inner.borrow_mut().fatal(msg)
    }
    pub fn err(&self, msg: &str) {
        self.inner.borrow_mut().err(msg);
    }
    pub fn warn(&self, msg: &str) {
        let mut db = DiagnosticBuilder::new(self, Warning, msg);
        db.emit();
    }
    pub fn note_without_error(&self, msg: &str) {
        let mut db = DiagnosticBuilder::new(self, Note, msg);
        db.emit();
    }
    pub fn bug(&self, msg: &str) -> ! {
        self.inner.borrow_mut().bug(msg)
    }

    pub fn err_count(&self) -> usize {
        self.inner.borrow().err_count
    }

    pub fn has_errors(&self) -> bool {
        self.err_count() > 0
    }

    pub fn print_error_count(&self, registry: &Registry) {
        self.inner.borrow_mut().print_error_count(registry)
    }

    pub fn abort_if_errors(&self) {
        self.inner.borrow().abort_if_errors()
    }

    pub fn abort_if_errors_and_should_abort(&self) {
        self.inner.borrow().abort_if_errors_and_should_abort()
    }

    pub fn must_teach(&self, code: &DiagnosticId) -> bool {
        self.inner.borrow_mut().must_teach(code)
    }

    pub fn force_print_diagnostic(&self, db: Diagnostic) {
        self.inner.borrow_mut().force_print_diagnostic(db)
    }

    pub fn emit_diagnostic(&self, diagnostic: &Diagnostic) {
        self.inner.borrow_mut().emit_diagnostic(diagnostic)
    }

    pub fn emit_artifact_notification(&self, path: &Path, artifact_type: &str) {
        self.inner.borrow_mut().emit_artifact_notification(path, artifact_type)
    }

    pub fn delay_as_bug(&self, diagnostic: Diagnostic) {
        self.inner.borrow_mut().delay_as_bug(diagnostic)
    }
}

impl HandlerInner {
    /// `true` if we haven't taught a diagnostic with this code already.
    /// The caller must then teach the user about such a diagnostic.
    ///
    /// Used to suppress emitting the same error multiple times with extended explanation when
    /// calling `-Zteach`.
    fn must_teach(&mut self, code: &DiagnosticId) -> bool {
        self.taught_diagnostics.insert(code.clone())
    }

    fn force_print_diagnostic(&mut self, db: Diagnostic) {
        self.emitter.emit_diagnostic(&db);
    }

    fn emit_diagnostic(&mut self, diagnostic: &Diagnostic) {
        if diagnostic.cancelled() {
            return;
        }

        if diagnostic.level == Warning && !self.flags.can_emit_warnings {
            return;
        }

        TRACK_DIAGNOSTICS.with(|track_diagnostics| {
            track_diagnostics.get()(diagnostic);
        });

        if let Some(ref code) = diagnostic.code {
            self.emitted_diagnostic_codes.insert(code.clone());
        }

        let diagnostic_hash = {
            use std::hash::Hash;
            let mut hasher = StableHasher::new();
            diagnostic.hash(&mut hasher);
            hasher.finish()
        };

        // Only emit the diagnostic if we haven't already emitted an equivalent
        // one:
        if self.emitted_diagnostics.insert(diagnostic_hash) {
            self.emitter.emit_diagnostic(diagnostic);
            if diagnostic.is_error() {
                self.deduplicated_err_count += 1;
            }
        }
        if diagnostic.is_error() {
            self.bump_err_count();
        }
    }

    fn emit_artifact_notification(&mut self, path: &Path, artifact_type: &str) {
        self.emitter.emit_artifact_notification(path, artifact_type);
    }

    fn treat_err_as_bug(&self) -> bool {
        self.flags.treat_err_as_bug.map(|c| self.err_count >= c).unwrap_or(false)
    }

    fn print_error_count(&mut self, registry: &Registry) {
        let s = match self.deduplicated_err_count {
            0 => return,
            1 => "aborting due to previous error".to_string(),
            count => format!("aborting due to {} previous errors", count)
        };
        if self.treat_err_as_bug() {
            return;
        }

        let _ = self.fatal(&s);

        let can_show_explain = self.emitter.should_show_explain();
        let are_there_diagnostics = !self.emitted_diagnostic_codes.is_empty();
        if can_show_explain && are_there_diagnostics {
            let mut error_codes = self
                .emitted_diagnostic_codes
                .iter()
                .filter_map(|x| match &x {
                    DiagnosticId::Error(s) if registry.find_description(s).is_some() => {
                        Some(s.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if !error_codes.is_empty() {
                error_codes.sort();
                if error_codes.len() > 1 {
                    let limit = if error_codes.len() > 9 { 9 } else { error_codes.len() };
                    self.failure(&format!("Some errors have detailed explanations: {}{}",
                                          error_codes[..limit].join(", "),
                                          if error_codes.len() > 9 { "..." } else { "." }));
                    self.failure(&format!("For more information about an error, try \
                                           `rustc --explain {}`.",
                                          &error_codes[0]));
                } else {
                    self.failure(&format!("For more information about this error, try \
                                           `rustc --explain {}`.",
                                          &error_codes[0]));
                }
            }
        }
    }

    fn abort_if_errors_and_should_abort(&self) {
        if self.err_count > 0 && !self.continue_after_error {
            FatalError.raise();
        }
    }

    fn abort_if_errors(&self) {
        if self.err_count > 0 {
            FatalError.raise();
        }
    }

    fn span_bug<S: Into<MultiSpan>>(&mut self, sp: S, msg: &str) -> ! {
        self.emit_diagnostic(Diagnostic::new(Bug, msg).set_span(sp));
        self.abort_if_errors_and_should_abort();
        panic!(ExplicitBug);
    }

    fn delay_span_bug<S: Into<MultiSpan>>(&mut self, sp: S, msg: &str) {
        if self.treat_err_as_bug() {
            // FIXME: don't abort here if report_delayed_bugs is off
            self.span_bug(sp, msg);
        }
        let mut diagnostic = Diagnostic::new(Level::Bug, msg);
        diagnostic.set_span(sp.into());
        self.delay_as_bug(diagnostic)
    }

    fn failure(&mut self, msg: &str) {
        self.emit_diagnostic(&Diagnostic::new(FailureNote, msg));
    }

    fn fatal(&mut self, msg: &str) -> FatalError {
        if self.treat_err_as_bug() {
            self.bug(msg);
        }
        self.emit_diagnostic(&Diagnostic::new(Fatal, msg));
        FatalError
    }

    fn err(&mut self, msg: &str) {
        if self.treat_err_as_bug() {
            self.bug(msg);
        }
        self.emit_diagnostic(&Diagnostic::new(Error, msg));
    }

    fn bug(&mut self, msg: &str) -> ! {
        self.emit_diagnostic(&Diagnostic::new(Bug, msg));
        panic!(ExplicitBug);
    }

    fn delay_as_bug(&mut self, diagnostic: Diagnostic) {
        if self.flags.report_delayed_bugs {
            self.emit_diagnostic(&diagnostic);
        }
        self.delayed_span_bugs.push(diagnostic);
    }

    fn bump_err_count(&mut self) {
        self.err_count += 1;
        self.panic_if_treat_err_as_bug();
    }

    fn panic_if_treat_err_as_bug(&self) {
        if self.treat_err_as_bug() {
            let s = match (self.err_count, self.flags.treat_err_as_bug.unwrap_or(0)) {
                (0, _) => return,
                (1, 1) => "aborting due to `-Z treat-err-as-bug=1`".to_string(),
                (1, _) => return,
                (count, as_bug) => {
                    format!(
                        "aborting after {} errors due to `-Z treat-err-as-bug={}`",
                        count,
                        as_bug,
                    )
                }
            };
            panic!(s);
        }
    }
}

#[derive(Copy, PartialEq, Clone, Hash, Debug, RustcEncodable, RustcDecodable)]
pub enum Level {
    Bug,
    Fatal,
    Error,
    Warning,
    Note,
    Help,
    Cancelled,
    FailureNote,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_str().fmt(f)
    }
}

impl Level {
    fn color(self) -> ColorSpec {
        let mut spec = ColorSpec::new();
        match self {
            Bug | Fatal | Error => {
                spec.set_fg(Some(Color::Red))
                    .set_intense(true);
            }
            Warning => {
                spec.set_fg(Some(Color::Yellow))
                    .set_intense(cfg!(windows));
            }
            Note => {
                spec.set_fg(Some(Color::Green))
                    .set_intense(true);
            }
            Help => {
                spec.set_fg(Some(Color::Cyan))
                    .set_intense(true);
            }
            FailureNote => {}
            Cancelled => unreachable!(),
        }
        spec
    }

    pub fn to_str(self) -> &'static str {
        match self {
            Bug => "error: internal compiler error",
            Fatal | Error => "error",
            Warning => "warning",
            Note => "note",
            Help => "help",
            FailureNote => "failure-note",
            Cancelled => panic!("Shouldn't call on cancelled error"),
        }
    }

    pub fn is_failure_note(&self) -> bool {
        match *self {
            FailureNote => true,
            _ => false,
        }
    }
}

#[macro_export]
macro_rules! pluralise {
    ($x:expr) => {
        if $x != 1 { "s" } else { "" }
    };
}
