use serde::{Deserialize, Serialize};
use turborepo_vercel_api::{TelemetryEvent, TelemetryTaskEvent};
use uuid::Uuid;

use super::{Event, EventBuilder, EventType, Identifiable};
use crate::{config::TelemetryConfig, telem};

// task names that will be passed through to the API without obfuscation
const ALLOWLIST: [&str; 8] = [
    "build",
    "test",
    "lint",
    "typecheck",
    "checktypes",
    "check-types",
    "type-check",
    "check",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageTaskEventBuilder {
    id: String,
    package: String,
    task: String,
    parent_id: Option<String>,
}

impl Identifiable for PackageTaskEventBuilder {
    fn get_id(&self) -> &String {
        &self.id
    }
}

impl EventBuilder for PackageTaskEventBuilder {
    fn with_parent<U: Identifiable>(mut self, parent_event: &U) -> Self {
        self.parent_id = Some(parent_event.get_id().clone());
        self
    }

    fn track(&self, event: Event) {
        let val = match event.is_sensitive {
            EventType::Sensitive => TelemetryConfig::one_way_hash(&event.value),
            EventType::NonSensitive => event.value.to_string(),
        };

        telem(TelemetryEvent::Task(TelemetryTaskEvent {
            id: self.id.clone(),
            package: self.package.clone(),
            task: self.task.clone(),
            parent_id: self.parent_id.clone(),
            key: event.key,
            value: val,
        }));
    }

    fn child(&self) -> Self {
        Self::new(&self.package, &self.task).with_parent(self)
    }
}

impl PackageTaskEventBuilder {
    pub fn new(package: &str, task: &str) -> Self {
        // don't obfuscate the package in development mode
        let package = if cfg!(debug_assertions) {
            package.to_string()
        } else {
            TelemetryConfig::one_way_hash(package)
        };

        // don't obfuscate the task in development mode or if it's in the allowlist
        let task = if cfg!(debug_assertions) || ALLOWLIST.contains(&task) {
            task.to_string()
        } else {
            TelemetryConfig::one_way_hash(task)
        };

        Self {
            id: Uuid::new_v4().to_string(),
            parent_id: None,
            package,
            task,
        }
    }

    // event methods
    pub fn track_recursive_error(&self) -> &Self {
        self.track(Event {
            key: "error".to_string(),
            value: "recursive".to_string(),
            is_sensitive: EventType::NonSensitive,
        });
        self
    }

    pub fn track_framework(&self, framework: &str) -> &Self {
        self.track(Event {
            key: "framework".to_string(),
            value: framework.to_string(),
            is_sensitive: EventType::NonSensitive,
        });
        self
    }
}
