use super::mode::MutationIntent;
use super::*;

impl<T: ControlTransport> HandoffService<T> {
    pub(super) fn record_lease(&self, id: &str) -> Result<Option<std::fs::File>> {
        Database::new(&self.config.database_path)
            .try_content_lease(&format!("peerlume-handoff:{id}"))
            .map_err(|_| Error::Persistence)
    }

    /// Continues only mode/start steps authorized by a persisted confirmation.
    /// Never retries Add or an unacknowledged mutation, including after a crash.
    pub fn advance_setups(&self) -> Result<Vec<Handoff>> {
        let records = self.refresh()?;
        if !records.iter().any(Handoff::pending_setup) {
            return Ok(records);
        }
        let (instance, discovery) = client::discover_session(&self.transport)?;
        let snapshot = self.snapshot(&instance)?;
        for record in records.iter().filter(|record| record.pending_setup()) {
            let Some(_lease) = self.record_lease(&record.id)? else {
                continue;
            };
            let mut record = self.journal.get(&record.id)?;
            if !record.pending_setup() {
                continue;
            }
            if record.instance != instance {
                finish(
                    &mut record,
                    SetupState::ReconciliationRequired,
                    Error::OwnerChanged,
                );
            } else if let Err(error) = client::require_mode_grant(&discovery, &record.download_mode)
            {
                finish(&mut record, SetupState::Failed, error);
            } else {
                let row = snapshot.iter().find(|row| record.hashes.matches(&row.id));
                if let Err(error) = self.advance_one(&mut record, row, &instance) {
                    finish(&mut record, SetupState::ReconciliationRequired, error);
                }
            }
            self.journal.save(&record)?;
        }
        self.history()
    }

    fn advance_one(
        &self,
        record: &mut Handoff,
        row: Option<&Observation>,
        instance: &str,
    ) -> Result<()> {
        let phase = record.mode_setup.as_ref().ok_or(Error::Protocol)?.state;
        match phase {
            SetupState::AwaitingAdd => match record.state {
                HandoffState::Added if record.operation_id.is_some() => {
                    if now_ms() >= record.deadline_unix_ms {
                        finish(record, SetupState::Failed, Error::SetupExpired);
                    } else if let Some(row) = row {
                        if row.state == TransferState::Checking {
                            return Ok(());
                        } else if safe_to_configure(row) {
                            self.send_mutation(record, row, instance, false)?;
                        } else {
                            finish(record, SetupState::Failed, Error::ModeChanged);
                        }
                    }
                }
                HandoffState::Failed => finish(record, SetupState::Failed, Error::Remote),
                HandoffState::Linked | HandoffState::ReconciliationRequired => {
                    finish(
                        record,
                        SetupState::ReconciliationRequired,
                        Error::ReconciliationRequired,
                    );
                }
                _ => (),
            },
            SetupState::ApplyingMode | SetupState::Starting => {
                let starting = phase == SetupState::Starting;
                let setup = record.mode_setup.as_ref().unwrap();
                let intent = if starting {
                    &setup.start_request
                } else {
                    &setup.mode_request
                };
                let id = intent
                    .as_ref()
                    .and_then(|intent| intent.operation_id.as_deref())
                    .ok_or(Error::ReconciliationRequired)?;
                let (_, operation): (_, Operation) = call(
                    &self.transport,
                    Some(instance),
                    json!({"name":"operation","args":{"operation_id":id}}),
                )?;
                let expected = if starting { "resume" } else { "set_mode" };
                validate_operation(record, &operation, expected)?;
                if operation.operation_id != id {
                    return Err(Error::Protocol);
                }
                match operation.state {
                    OperationState::Queued | OperationState::Running => (),
                    OperationState::Failed | OperationState::Cancelled => finish(
                        record,
                        SetupState::Failed,
                        operation
                            .error
                            .as_deref()
                            .map_or(Error::Remote, Error::remote),
                    ),
                    OperationState::ReconciliationRequired => finish(
                        record,
                        SetupState::ReconciliationRequired,
                        Error::ReconciliationRequired,
                    ),
                    OperationState::Succeeded if starting => {
                        record.mode_setup.as_mut().unwrap().state = SetupState::Complete;
                        record.error = None;
                    }
                    OperationState::Succeeded => {
                        let Some(row) = row else {
                            finish(record, SetupState::Failed, Error::ModeChanged);
                            return Ok(());
                        };
                        // The operation proves the engine acknowledgement; a fresh
                        // snapshot also prevents resuming a subsequently changed mode.
                        if row.mode_state.as_deref() != Some("active") {
                            return Ok(());
                        }
                        if !mode_matches(row, &record.download_mode) {
                            finish(record, SetupState::Failed, Error::ModeChanged);
                        } else if !record.start {
                            record.mode_setup.as_mut().unwrap().state = SetupState::Complete;
                            record.error = None;
                        } else if now_ms() >= record.deadline_unix_ms {
                            finish(record, SetupState::Failed, Error::SetupExpired);
                        } else if row.state == TransferState::Checking {
                            return Ok(());
                        } else if safe_to_configure(row) {
                            self.send_mutation(record, row, instance, true)?;
                        } else {
                            finish(record, SetupState::Failed, Error::ModeChanged);
                        }
                    }
                }
            }
            _ => (),
        }
        Ok(())
    }

    fn send_mutation(
        &self,
        record: &mut Handoff,
        row: &Observation,
        instance: &str,
        starting: bool,
    ) -> Result<()> {
        let intent = MutationIntent {
            expected_revision: row.command_revision,
            idempotency_key: random_id()?,
            deadline_unix_ms: record.deadline_unix_ms,
            operation_id: None,
        };
        let setup = record.mode_setup.as_mut().ok_or(Error::Protocol)?;
        if starting {
            setup.state = SetupState::Starting;
            setup.start_request = Some(intent.clone());
        } else {
            setup.state = SetupState::ApplyingMode;
            setup.mode_request = Some(intent.clone());
        }
        // The exact command revision/key/deadline survive an ambiguous delivery.
        self.journal.save(record)?;
        let command = if starting {
            json!({"kind":"resume"})
        } else {
            json!({"kind":"set_mode","value":record.download_mode})
        };
        let (_, operation): (_, Operation) = call(
            &self.transport,
            Some(instance),
            json!({"name":"mutate","args":{
                "torrent_id":row.id,"command":command,"expected_revision":intent.expected_revision,
                "idempotency_key":intent.idempotency_key,"deadline_unix_ms":intent.deadline_unix_ms
            }}),
        )?;
        validate_operation(
            record,
            &operation,
            if starting { "resume" } else { "set_mode" },
        )?;
        let setup = record.mode_setup.as_mut().unwrap();
        let saved = if starting {
            setup.start_request.as_mut()
        } else {
            setup.mode_request.as_mut()
        }
        .ok_or(Error::Protocol)?;
        saved.operation_id = Some(operation.operation_id);
        match operation.state {
            OperationState::Failed | OperationState::Cancelled => finish(
                record,
                SetupState::Failed,
                operation
                    .error
                    .as_deref()
                    .map_or(Error::Remote, Error::remote),
            ),
            OperationState::ReconciliationRequired => finish(
                record,
                SetupState::ReconciliationRequired,
                Error::ReconciliationRequired,
            ),
            // Even immediate mode success needs a fresh observed configuration.
            OperationState::Succeeded if starting => {
                record.mode_setup.as_mut().unwrap().state = SetupState::Complete;
                record.error = None;
            }
            _ => (),
        }
        Ok(())
    }
}

fn finish(record: &mut Handoff, state: SetupState, error: Error) {
    if let Some(setup) = &mut record.mode_setup {
        setup.state = state;
    }
    record.error = Some(error.to_string());
}
fn safe_to_configure(row: &Observation) -> bool {
    matches!(
        row.state,
        TransferState::Paused | TransferState::AwaitingEngine
    )
}
fn mode_matches(row: &Observation, config: &ModeConfig) -> bool {
    row.active_mode == Some(config.mode)
        && (config.mode != DownloadMode::FreezeAfter
            || row.active_freeze_after_bytes == Some(config.freeze_after_bytes))
        && (config.mode != DownloadMode::UploadRatio
            || row.active_upload_ratio == Some(config.upload_ratio))
}
fn validate_operation(record: &Handoff, operation: &Operation, expected: &str) -> Result<()> {
    if operation.command != expected
        || operation.operation_id.is_empty()
        || operation.operation_id.len() > 128
        || !record.hashes.matches(&operation.torrent_id)
    {
        return Err(Error::Protocol);
    }
    Ok(())
}
