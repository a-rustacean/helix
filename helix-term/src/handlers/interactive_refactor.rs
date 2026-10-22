use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use helix_lsp::{jsonrpc, lsp, LanguageServerId};
use helix_stdx::Url;
use helix_view::Editor;
use serde_json::Value;

use crate::compositor::Compositor;
use crate::job::dispatch_blocking;
use crate::ui::{self, menu, PromptEvent};

/// Identifies the most recently started flow. Responses for superseded flows are dropped so a
/// slow resolve cannot surface prompts after the user has moved on or started another flow.
static CURRENT_FLOW: AtomicU64 = AtomicU64::new(0);

/// Maximum number of prompting rounds before giving up on a server that never stops asking.
const MAX_RESOLVE_ROUNDS: usize = 10;

fn is_current(flow: u64) -> bool {
    CURRENT_FLOW.load(Ordering::Relaxed) == flow
}

/// Starts the interactive resolution flow for a command.
///
/// The command is first sent to `command/resolve`. If the server responds with form fields, the
/// user is prompted for answers one field at a time and the command params are re-resolved until
/// the server returns no more fields. The command is then executed via `workspace/executeCommand`.
pub fn start(server_id: LanguageServerId, command: lsp::Command, editor: &mut Editor) {
    let flow = CURRENT_FLOW.fetch_add(1, Ordering::Relaxed) + 1;

    let params = lsp::ExecuteCommandParams {
        command: command.command,
        arguments: command.arguments.unwrap_or_default(),
        work_done_progress_params: Default::default(),
        interactive: Default::default(),
    };

    // `rounds` counts the resolve round trips; the first one is in flight as of this call.
    resolve(server_id, params, flow, 1, editor);
}

/// Sends the current params to `command/resolve` and continues the flow with the response.
///
/// The request must not block the main loop: servers may send requests of their own while
/// resolving the command (for example `window/workDoneProgress/create`) and wait for the client
/// to respond before answering. Blocking here would deadlock until the request times out.
fn resolve(
    server_id: LanguageServerId,
    mut params: lsp::ExecuteCommandParams,
    flow: u64,
    rounds: usize,
    editor: &mut Editor,
) {
    let Some(language_server) = editor.language_server_by_id(server_id) else {
        editor.set_error("Language server disappeared");
        return;
    };

    // `formFields` is only defined for server responses, never requests.
    params.interactive.form_fields = None;

    let Some(future) = language_server.resolve_command(params.clone()) else {
        editor.set_error("Language server does not support interactive command resolution");
        return;
    };

    tokio::spawn(async move {
        let result = future.await;
        dispatch_blocking(move |editor, compositor| {
            if !is_current(flow) {
                log::debug!("dropping response for a superseded interactive resolve flow");
                return;
            }

            match result {
                Ok(params) => handle_response(server_id, params, flow, rounds, editor, compositor),
                // A server may advertise command/resolve but still reject individual commands,
                // for example when a proxy strips the method. Execute the command as if the
                // server had not offered interactivity instead of failing the action outright.
                Err(err) if is_method_not_found(&err) => {
                    log::debug!(
                        "command/resolve is not supported for this command, executing it directly"
                    );
                    let command = lsp::Command {
                        title: String::new(),
                        command: params.command,
                        arguments: Some(params.arguments),
                    };
                    editor.execute_lsp_command(command, server_id);
                }
                Err(err) => editor.set_error(format!("Failed to resolve command: {err}")),
            }
        });
    });
}

/// Whether the error is the JSON-RPC `Method not found` (-32601) error.
fn is_method_not_found(error: &helix_lsp::Error) -> bool {
    matches!(
        error,
        helix_lsp::Error::Rpc(error) if error.code == jsonrpc::ErrorCode::MethodNotFound
    )
}

/// Handles a response from `command/resolve`, either by asking the remaining questions or by
/// executing the command once the server has stopped asking for input.
fn handle_response(
    server_id: LanguageServerId,
    mut params: lsp::ExecuteCommandParams,
    flow: u64,
    rounds: usize,
    editor: &mut Editor,
    compositor: &mut Compositor,
) {
    let fields = params
        .interactive
        .form_fields
        .take()
        .filter(|fields| !fields.is_empty());

    let Some(fields) = fields else {
        execute(server_id, params, editor);
        return;
    };

    // Guard against a server that returns questions forever instead of converging; without a
    // limit the user would be stuck in an unbounded loop of prompts.
    if rounds > MAX_RESOLVE_ROUNDS {
        editor.set_error(format!(
            "The language server keeps requesting input; giving up after {MAX_RESOLVE_ROUNDS} rounds"
        ));
        return;
    }

    // Answers for fields that the server no longer asks about must not be sent back.
    if let Some(answers) = &mut params.interactive.form_answers {
        answers.retain(|answer| fields.iter().any(|field| field.id == answer.id));
    }

    let mut pending: VecDeque<_> = fields
        .iter()
        .filter(|field| needs_answer(field, &params))
        .cloned()
        .collect();

    if pending.is_empty() {
        // The server asked for another round even though every field already has an answer.
        // Present all fields again so the user can adjust their answers instead of looping
        // through resolutions without any interaction.
        pending = fields.into();
    }

    ask_next(server_id, params, flow, rounds, pending, editor, compositor);
}

/// Whether a field should be presented to the user. Answered fields are skipped unless the
/// server reported a validation error for them.
fn needs_answer(field: &lsp::FormField, params: &lsp::ExecuteCommandParams) -> bool {
    if field
        .error
        .as_deref()
        .is_some_and(|error| !error.is_empty())
    {
        return true;
    }

    !params
        .interactive
        .form_answers
        .as_ref()
        .is_some_and(|answers| answers.iter().any(|answer| answer.id == field.id))
}

fn ask_next(
    server_id: LanguageServerId,
    params: lsp::ExecuteCommandParams,
    flow: u64,
    rounds: usize,
    mut pending: VecDeque<lsp::FormField>,
    editor: &mut Editor,
    compositor: &mut Compositor,
) {
    let Some(field) = pending.pop_front() else {
        resolve(server_id, params, flow, rounds + 1, editor);
        return;
    };

    if let Some(error) = field.error.as_deref().filter(|error| !error.is_empty()) {
        editor.set_error(format!("{}: {error}", field.description));
    }

    match field.field_type.clone() {
        lsp::FormFieldType::String => ask_text(
            server_id,
            params,
            flow,
            rounds,
            pending,
            field,
            ui::completers::none,
            editor,
            compositor,
        ),
        lsp::FormFieldType::File { .. } => {
            let completer = file_completer(&field);
            ask_text(
                server_id, params, flow, rounds, pending, field, completer, editor, compositor,
            )
        }
        lsp::FormFieldType::Number => ask_text(
            server_id,
            params,
            flow,
            rounds,
            pending,
            field,
            ui::completers::none,
            editor,
            compositor,
        ),
        lsp::FormFieldType::Bool => {
            ask_bool(server_id, params, flow, rounds, pending, field, compositor)
        }
        lsp::FormFieldType::Enum { entries, .. } if entries.is_empty() => {
            editor.set_error(format!(
                "Language server sent no options for \"{}\"",
                field.description
            ));
        }
        lsp::FormFieldType::Enum { entries, .. } => ask_enum(
            server_id, params, flow, rounds, pending, field, &entries, compositor,
        ),
        lsp::FormFieldType::LazyEnum { .. } | lsp::FormFieldType::List { .. } => {
            editor.set_error(format!(
                "Unsupported form field type for \"{}\"",
                field.description
            ));
        }
    }
}

fn ask_text(
    server_id: LanguageServerId,
    params: lsp::ExecuteCommandParams,
    flow: u64,
    rounds: usize,
    pending: VecDeque<lsp::FormField>,
    field: lsp::FormField,
    completer: ui::completers::Completer,
    editor: &mut Editor,
    compositor: &mut Compositor,
) {
    let prefill = match &field.field_type {
        lsp::FormFieldType::File { .. } => initial_value(&params, &field)
            .map(|value| file_input_value(&value))
            .unwrap_or_default(),
        _ => initial_value(&params, &field).unwrap_or_default(),
    };

    let prompt = ui::Prompt::new(
        prompt_text(&field),
        None,
        completer,
        move |_cx: &mut crate::compositor::Context, input: &str, event: PromptEvent| {
            if event != PromptEvent::Validate {
                return;
            }

            match text_answer(input, &field) {
                Ok(value) => answer_and_continue(
                    server_id,
                    params.clone(),
                    flow,
                    rounds,
                    pending.clone(),
                    &field,
                    value,
                ),
                Err(err) => {
                    let params = params.clone();
                    let mut pending = pending.clone();
                    pending.push_front(field.clone());
                    dispatch_blocking(move |editor, compositor| {
                        editor.set_error(err);
                        ask_next(server_id, params, flow, rounds, pending, editor, compositor);
                    });
                }
            }
        },
    )
    .with_line(prefill, editor);

    compositor.push(Box::new(prompt));
}

fn ask_bool(
    server_id: LanguageServerId,
    params: lsp::ExecuteCommandParams,
    flow: u64,
    rounds: usize,
    pending: VecDeque<lsp::FormField>,
    field: lsp::FormField,
    compositor: &mut Compositor,
) {
    let value = initial_bool(&params, &field);
    let selection = if value { 0 } else { 1 };

    let select = ui::Select::new(
        prompt_text(&field),
        [BoolChoice(true), BoolChoice(false)],
        (),
        move |_editor: &mut Editor, choice: &BoolChoice, event: PromptEvent| {
            if event != PromptEvent::Validate {
                return;
            }
            answer_and_continue(
                server_id,
                params.clone(),
                flow,
                rounds,
                pending.clone(),
                &field,
                Value::Bool(choice.0),
            );
        },
    )
    .with_selection(selection);

    compositor.push(Box::new(select));
}

fn ask_enum(
    server_id: LanguageServerId,
    params: lsp::ExecuteCommandParams,
    flow: u64,
    rounds: usize,
    pending: VecDeque<lsp::FormField>,
    field: lsp::FormField,
    entries: &[lsp::FormEnumEntry],
    compositor: &mut Compositor,
) {
    let selection = initial_enum(&params, &field, entries);
    let options = entries
        .iter()
        .map(|entry| EnumChoice {
            value: entry.value.clone(),
            label: if entry.description.is_empty() || entry.description == entry.value {
                entry.value.clone()
            } else {
                format!("{} ({})", entry.description, entry.value)
            },
        })
        .collect::<Vec<_>>();

    let select = ui::Select::new(
        prompt_text(&field),
        options,
        (),
        move |_editor: &mut Editor, choice: &EnumChoice, event: PromptEvent| {
            if event != PromptEvent::Validate {
                return;
            }
            answer_and_continue(
                server_id,
                params.clone(),
                flow,
                rounds,
                pending.clone(),
                &field,
                Value::String(choice.value.clone()),
            );
        },
    )
    .with_selection(selection);

    compositor.push(Box::new(select));
}

/// Records an answer and moves on to the next field, or resolves the command when there are no
/// fields left.
fn answer_and_continue(
    server_id: LanguageServerId,
    mut params: lsp::ExecuteCommandParams,
    flow: u64,
    rounds: usize,
    pending: VecDeque<lsp::FormField>,
    field: &lsp::FormField,
    value: Value,
) {
    set_answer(&mut params, &field.id, value);

    dispatch_blocking(move |editor, compositor| {
        ask_next(server_id, params, flow, rounds, pending, editor, compositor);
    });
}

fn execute(
    server_id: LanguageServerId,
    mut params: lsp::ExecuteCommandParams,
    editor: &mut Editor,
) {
    params.interactive.form_fields = None;

    let Some(language_server) = editor.language_server_by_id(server_id) else {
        editor.set_error("Language server disappeared");
        return;
    };

    let Some(future) = language_server.execute_command(params) else {
        editor.set_error("Language server does not support executing commands");
        return;
    };

    tokio::spawn(async move {
        if let Err(err) = future.await {
            log::error!("Error executing LSP command: {err}");
        }
    });
}

fn set_answer(params: &mut lsp::ExecuteCommandParams, id: &str, value: Value) {
    let answers = params.interactive.form_answers.get_or_insert_with(Vec::new);

    match answers.iter_mut().find(|answer| answer.id == id) {
        Some(answer) => answer.value = value,
        None => answers.push(lsp::FormAnswer {
            id: id.to_string(),
            value,
        }),
    }
}

fn prompt_text(field: &lsp::FormField) -> Cow<'static, str> {
    match field.error.as_deref().filter(|error| !error.is_empty()) {
        Some(error) => Cow::Owned(format!("{} ({}): ", field.description, error)),
        None => Cow::Owned(format!("{}: ", field.description)),
    }
}

fn value_to_input(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        value => value.to_string(),
    }
}

fn file_input_value(value: &str) -> String {
    Url::parse(value)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| value.to_string())
}

/// The answer currently stored for a field, ignoring `null` values.
fn current_value<'a>(
    params: &'a lsp::ExecuteCommandParams,
    field: &lsp::FormField,
) -> Option<&'a Value> {
    params
        .interactive
        .form_answers
        .as_ref()?
        .iter()
        .find(|answer| answer.id == field.id)
        .map(|answer| &answer.value)
        .filter(|value| !value.is_null())
}

fn initial_value(params: &lsp::ExecuteCommandParams, field: &lsp::FormField) -> Option<String> {
    current_value(params, field)
        .or(field.default.as_ref().filter(|value| !value.is_null()))
        .map(value_to_input)
}

fn initial_bool(params: &lsp::ExecuteCommandParams, field: &lsp::FormField) -> bool {
    current_value(params, field)
        .or(field.default.as_ref())
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn initial_enum(
    params: &lsp::ExecuteCommandParams,
    field: &lsp::FormField,
    entries: &[lsp::FormEnumEntry],
) -> usize {
    current_value(params, field)
        .or(field.default.as_ref())
        .and_then(Value::as_str)
        .and_then(|value| entries.iter().position(|entry| entry.value == value))
        .unwrap_or(0)
}

fn text_answer(input: &str, field: &lsp::FormField) -> Result<Value, String> {
    if input.is_empty() {
        if field.required {
            return Err("A value is required".to_string());
        }
        return Ok(Value::Null);
    }

    match &field.field_type {
        lsp::FormFieldType::String => Ok(Value::String(input.to_string())),
        lsp::FormFieldType::Number => {
            if let Ok(number) = input.parse::<i64>() {
                return Ok(Value::Number(number.into()));
            }

            input
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| "Expected a number".to_string())
        }
        lsp::FormFieldType::File { .. } => file_answer(input, field),
        _ => Err("Unsupported form field type".to_string()),
    }
}

fn file_answer(input: &str, field: &lsp::FormField) -> Result<Value, String> {
    let lsp::FormFieldType::File {
        existence,
        file_type,
        filters,
    } = &field.field_type
    else {
        return Err("Unsupported form field type".to_string());
    };

    let path = Path::new(input);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| "Could not resolve the path".to_string())?
            .join(path)
    };

    let exists = path.exists();
    let is_dir = path.is_dir();

    let existence = existence.unwrap_or(lsp::FileExistence::NEW | lsp::FileExistence::EXISTING);
    if existence.bits() != 0 {
        if exists && !existence.contains(lsp::FileExistence::EXISTING) {
            return Err("The file already exists".to_string());
        }
        if !exists && !existence.contains(lsp::FileExistence::NEW) {
            return Err("The file does not exist".to_string());
        }
    }

    if exists {
        let file_type = file_type.unwrap_or(lsp::FileType::REGULAR);
        if file_type.bits() != 0 {
            let allowed = (is_dir && file_type.contains(lsp::FileType::DIRECTORY))
                || (!is_dir && file_type.contains(lsp::FileType::REGULAR));
            if !allowed {
                return Err(if is_dir {
                    "Expected a file, found a directory".to_string()
                } else {
                    "Expected a directory, found a file".to_string()
                });
            }
        }
    }

    if !is_dir {
        if let Some(filters) = filters.as_deref().filter(|filters| !filters.is_empty()) {
            let extension = path.extension().and_then(|extension| extension.to_str());
            let allowed = extension.is_some_and(|extension| {
                filters
                    .iter()
                    .any(|filter| filter.eq_ignore_ascii_case(extension))
            });
            if !allowed {
                return Err(format!(
                    "Expected a file with one of these extensions: {}",
                    filters.join(", ")
                ));
            }
        }
    }

    Url::from_file_path(&path)
        .map(|url| Value::String(url.to_string()))
        .map_err(|_| "Could not convert the path to a file URI".to_string())
}

fn file_completer(field: &lsp::FormField) -> ui::completers::Completer {
    match &field.field_type {
        lsp::FormFieldType::File {
            file_type: Some(file_type),
            ..
        } if !file_type.contains(lsp::FileType::REGULAR)
            && file_type.contains(lsp::FileType::DIRECTORY) =>
        {
            ui::completers::directory
        }
        _ => ui::completers::filename,
    }
}

struct BoolChoice(bool);

impl menu::Item for BoolChoice {
    type Data = ();

    fn format(&self, _data: &Self::Data) -> menu::Row<'_> {
        menu::Row::new(vec![menu::Cell::from(if self.0 {
            "true"
        } else {
            "false"
        })])
    }
}

struct EnumChoice {
    value: String,
    label: String,
}

impl menu::Item for EnumChoice {
    type Data = ();

    fn format(&self, _data: &Self::Data) -> menu::Row<'_> {
        menu::Row::new(vec![menu::Cell::from(self.label.as_str())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(
        field_type: lsp::FormFieldType,
        required: bool,
        default: Option<Value>,
    ) -> lsp::FormField {
        lsp::FormField {
            id: "test".into(),
            description: "test".into(),
            field_type,
            required,
            default,
            error: None,
        }
    }

    #[test]
    fn optional_empty_text_is_null() {
        let field = field(lsp::FormFieldType::String, false, None);
        assert_eq!(text_answer("", &field).unwrap(), Value::Null);
    }

    #[test]
    fn required_empty_text_is_rejected() {
        let field = field(lsp::FormFieldType::String, true, None);
        assert!(text_answer("", &field).is_err());
    }

    #[test]
    fn parses_numbers() {
        let field = field(lsp::FormFieldType::Number, true, None);
        assert_eq!(text_answer("42", &field).unwrap(), Value::from(42));
        assert_eq!(text_answer("1.5", &field).unwrap(), Value::from(1.5));
        assert!(text_answer("nope", &field).is_err());
    }

    #[test]
    fn file_existence_is_validated() {
        let field = field(
            lsp::FormFieldType::File {
                existence: Some(lsp::FileExistence::NEW),
                file_type: None,
                filters: None,
            },
            true,
            None,
        );

        let existing = std::env::current_exe().unwrap();
        assert!(text_answer(existing.to_str().unwrap(), &field).is_err());

        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("new.go");
        let answer = text_answer(new.to_str().unwrap(), &field).unwrap();
        assert!(answer.as_str().unwrap().starts_with("file://"));
    }

    #[test]
    fn detects_method_not_found() {
        let not_found = helix_lsp::Error::Rpc(jsonrpc::Error {
            code: jsonrpc::ErrorCode::MethodNotFound,
            message: String::new(),
            data: None,
        });
        assert!(is_method_not_found(&not_found));

        let other = helix_lsp::Error::Rpc(jsonrpc::Error {
            code: jsonrpc::ErrorCode::InternalError,
            message: String::new(),
            data: None,
        });
        assert!(!is_method_not_found(&other));
    }

    #[test]
    fn needs_answer_for_missing_and_invalid_fields() {
        let mut field = field(lsp::FormFieldType::String, true, None);
        let mut params = lsp::ExecuteCommandParams {
            command: "test".into(),
            ..Default::default()
        };
        assert!(needs_answer(&field, &params));

        params.interactive.form_answers = Some(vec![lsp::FormAnswer {
            id: field.id.clone(),
            value: Value::String("answer".into()),
        }]);
        assert!(!needs_answer(&field, &params));

        field.error = Some("invalid".into());
        assert!(needs_answer(&field, &params));
    }
}
