use serde::Serialize;
use std::time::Instant;

use super::writer::{diagnostic_sink, emit_diagnostic_to};

pub(crate) trait OperationOutcome {
    fn succeeded(&self) -> bool;
}

impl<T, E> OperationOutcome for Result<T, E> {
    fn succeeded(&self) -> bool {
        self.is_ok()
    }
}

impl OperationOutcome for () {
    fn succeeded(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Serialize)]
pub(crate) struct Operation<'a> {
    operation: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    element: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    factory: Option<&'a str>,
}

impl<'a> Operation<'a> {
    pub(crate) const fn named(operation: &'a str) -> Self {
        Self {
            operation,
            element: None,
            factory: None,
        }
    }

    pub(crate) const fn element(operation: &'a str, element: &'a str, factory: &'a str) -> Self {
        Self {
            operation,
            element: Some(element),
            factory: Some(factory),
        }
    }
}

#[derive(Serialize)]
struct OperationPayload<'a> {
    #[serde(flatten)]
    operation: Operation<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,
}

fn measure_operation_with<R: OperationOutcome>(
    operation: Operation<'_>,
    action: impl FnOnce() -> R,
    mut emit: impl FnMut(&str, OperationPayload<'_>),
) -> R {
    emit(
        "operation-started",
        OperationPayload {
            operation,
            duration_ms: None,
            success: None,
        },
    );
    let started = Instant::now();
    let result = action();
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let success = result.succeeded();
    emit(
        "operation-finished",
        OperationPayload {
            operation,
            duration_ms: Some(duration_ms),
            success: Some(success),
        },
    );
    result
}

pub(crate) fn measure_operation<R: OperationOutcome>(
    role: &str,
    operation: Operation<'_>,
    action: impl FnOnce() -> R,
) -> R {
    let Some(sink) = diagnostic_sink() else {
        return action();
    };
    measure_operation_with(operation, action, |event, payload| {
        emit_diagnostic_to(sink, event, role, payload);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_operation_preserves_success_value_and_reports_success() {
        let mut events = Vec::new();

        let result = measure_operation_with(
            Operation::named("incoming-video-pad-link"),
            || Ok::<_, &'static str>(String::from("unchanged")),
            |event, payload| {
                events.push((event.to_string(), serde_json::to_value(payload).unwrap()));
            },
        );

        assert_eq!(result, Ok(String::from("unchanged")));
        assert_eq!(events[0].0, "operation-started");
        assert_eq!(
            events[0].1,
            serde_json::json!({ "operation": "incoming-video-pad-link" })
        );
        assert_eq!(events[1].0, "operation-finished");
        assert_eq!(events[1].1["operation"], "incoming-video-pad-link");
        assert_eq!(events[1].1["success"], true);
        assert!(events[1].1["duration_ms"].is_u64());
    }

    #[test]
    fn measured_operation_preserves_error_and_reports_failure() {
        let mut events = Vec::new();

        let result = measure_operation_with(
            Operation::named("video-decoder-create-d3d11av1dec"),
            || Err::<String, _>("decoder unavailable"),
            |event, payload| {
                events.push((event.to_string(), serde_json::to_value(payload).unwrap()));
            },
        );

        assert_eq!(result, Err("decoder unavailable"));
        assert_eq!(events[0].0, "operation-started");
        assert_eq!(events[1].0, "operation-finished");
        assert_eq!(events[1].1["success"], false);
    }
}
