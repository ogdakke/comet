//! Deterministic provider used by engine, RPC, and conformance tests.

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;

use crate::{
    AccountBalance, CancelResult, GenerationRequest, MediaModel, MediaProvider, PollResult,
    ProviderAccount, ProviderAccountId, ProviderArtifact, ProviderError, ProviderErrorKind,
    ProviderId, ProviderResult, Quote, RemoteAttempt, RemoteJob, ResolvedInput, Secret, Submission,
    SubmissionCapabilities, SubmitContext,
};

#[derive(Clone, Debug)]
pub enum FakeSubmissionMode {
    Complete(Vec<ProviderArtifact>),
    Queue {
        polls_before_completion: usize,
        artifacts: Vec<ProviderArtifact>,
    },
    Fail(ProviderError),
}

#[derive(Debug)]
struct FakeJob {
    remaining_polls: usize,
    artifacts: Vec<ProviderArtifact>,
    cancelled: bool,
}

#[derive(Debug)]
struct State {
    next_job: u64,
    jobs: HashMap<String, FakeJob>,
    submissions: HashMap<String, Submission>,
    last_submit_inputs: Vec<ResolvedInput>,
}

/// A deliberately small configurable fake. It requires the secret `valid` by default.
#[derive(Debug)]
pub struct FakeMediaProvider {
    id: ProviderId,
    accepted_secret: String,
    models: Mutex<Vec<MediaModel>>,
    list_calls: AtomicUsize,
    quote: Option<Quote>,
    balance: Option<AccountBalance>,
    mode: FakeSubmissionMode,
    transient_polls: AtomicUsize,
    last_quote_reference_video_total: Mutex<Option<Option<f64>>>,
    complete_calls: AtomicUsize,
    state: Mutex<State>,
}

impl FakeMediaProvider {
    pub fn new(id: impl Into<String>, models: Vec<MediaModel>, mode: FakeSubmissionMode) -> Self {
        Self {
            id: ProviderId::new(id),
            accepted_secret: "valid".to_owned(),
            models: Mutex::new(models),
            list_calls: AtomicUsize::new(0),
            quote: None,
            balance: None,
            mode,
            transient_polls: AtomicUsize::new(0),
            last_quote_reference_video_total: Mutex::new(None),
            complete_calls: AtomicUsize::new(0),
            state: Mutex::new(State {
                next_job: 1,
                jobs: HashMap::new(),
                submissions: HashMap::new(),
                last_submit_inputs: Vec::new(),
            }),
        }
    }

    pub fn with_transient_polls(self, count: usize) -> Self {
        self.transient_polls.store(count, Ordering::SeqCst);
        self
    }

    pub fn last_quote_reference_video_total(&self) -> Option<Option<f64>> {
        *self
            .last_quote_reference_video_total
            .lock()
            .expect("fake provider lock poisoned")
    }

    pub fn complete_call_count(&self) -> usize {
        self.complete_calls.load(Ordering::SeqCst)
    }

    pub fn set_models(&self, models: Vec<MediaModel>) {
        *self.models.lock().expect("fake provider lock poisoned") = models;
    }

    pub fn list_call_count(&self) -> usize {
        self.list_calls.load(Ordering::SeqCst)
    }

    pub fn last_submit_inputs(&self) -> Vec<ResolvedInput> {
        self.state
            .lock()
            .expect("fake provider lock poisoned")
            .last_submit_inputs
            .clone()
    }

    pub fn with_accepted_secret(mut self, secret: impl Into<String>) -> Self {
        self.accepted_secret = secret.into();
        self
    }

    pub fn with_quote(mut self, quote: Quote) -> Self {
        self.quote = Some(quote);
        self
    }

    pub fn with_balance(mut self, balance: AccountBalance) -> Self {
        self.balance = Some(balance);
        self
    }

    fn authenticate(&self, secret: &Secret) -> ProviderResult<()> {
        if secret.expose() == self.accepted_secret {
            Ok(())
        } else {
            Err(ProviderError::new(
                ProviderErrorKind::InvalidCredential,
                "fake provider rejected the credential",
            ))
        }
    }
}

#[async_trait]
impl MediaProvider for FakeMediaProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn submission_capabilities(&self) -> SubmissionCapabilities {
        SubmissionCapabilities {
            accepts_idempotency_key: true,
            can_reconcile: true,
            supports_cancellation: true,
        }
    }

    async fn validate_credentials(&self, secret: &Secret) -> ProviderResult<ProviderAccount> {
        self.authenticate(secret)?;
        Ok(ProviderAccount {
            id: ProviderAccountId::new("fake-account"),
            label: "Fake account".to_owned(),
        })
    }

    async fn list_models(&self, secret: &Secret) -> ProviderResult<Vec<MediaModel>> {
        self.authenticate(secret)?;
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .models
            .lock()
            .expect("fake provider lock poisoned")
            .clone())
    }

    async fn quote(
        &self,
        secret: &Secret,
        _request: &GenerationRequest,
        reference_video_total_duration: Option<f64>,
    ) -> ProviderResult<Option<Quote>> {
        self.authenticate(secret)?;
        *self
            .last_quote_reference_video_total
            .lock()
            .expect("fake provider lock poisoned") = Some(reference_video_total_duration);
        Ok(self.quote.clone())
    }

    async fn balance(&self, secret: &Secret) -> ProviderResult<Option<AccountBalance>> {
        self.authenticate(secret)?;
        Ok(self.balance.clone())
    }

    async fn submit(
        &self,
        secret: &Secret,
        _request: &GenerationRequest,
        context: &SubmitContext,
    ) -> ProviderResult<Submission> {
        self.authenticate(secret)?;
        let mut state = self.state.lock().expect("fake provider lock poisoned");
        state.last_submit_inputs = context.inputs.clone();
        if let Some(submission) = state.submissions.get(&context.idempotency_key) {
            return Ok(submission.clone());
        }

        let submission = match &self.mode {
            FakeSubmissionMode::Complete(artifacts) => Submission::Completed {
                artifacts: artifacts.clone(),
            },
            FakeSubmissionMode::Queue {
                polls_before_completion,
                artifacts,
            } => {
                let job_id = format!("fake-job-{}", state.next_job);
                state.next_job += 1;
                state.jobs.insert(
                    job_id.clone(),
                    FakeJob {
                        remaining_polls: *polls_before_completion,
                        artifacts: artifacts.clone(),
                        cancelled: false,
                    },
                );
                Submission::Queued {
                    remote_job: RemoteJob {
                        id: job_id,
                        metadata: serde_json::Value::Null,
                    },
                }
            }
            FakeSubmissionMode::Fail(error) => return Err(error.clone()),
        };
        state
            .submissions
            .insert(context.idempotency_key.clone(), submission.clone());
        Ok(submission)
    }

    async fn reconcile(
        &self,
        secret: &Secret,
        attempt: &RemoteAttempt,
    ) -> ProviderResult<Option<Submission>> {
        self.authenticate(secret)?;
        Ok(self
            .state
            .lock()
            .expect("fake provider lock poisoned")
            .submissions
            .get(&attempt.idempotency_key)
            .cloned())
    }

    async fn poll(&self, secret: &Secret, remote_job: &RemoteJob) -> ProviderResult<PollResult> {
        self.authenticate(secret)?;
        if self.transient_polls.load(Ordering::SeqCst) > 0 {
            self.transient_polls.fetch_sub(1, Ordering::SeqCst);
            return Ok(PollResult::Failed {
                error: ProviderError::new(
                    ProviderErrorKind::Transient,
                    "fake provider transient poll error",
                ),
            });
        }
        let mut state = self.state.lock().expect("fake provider lock poisoned");
        let job = state.jobs.get_mut(&remote_job.id).ok_or_else(|| {
            ProviderError::new(ProviderErrorKind::InvalidRequest, "unknown fake job")
        })?;
        if job.cancelled {
            return Ok(PollResult::Failed {
                error: ProviderError::new(ProviderErrorKind::Other, "job was cancelled"),
            });
        }
        if job.remaining_polls > 0 {
            job.remaining_polls -= 1;
            return Ok(PollResult::Running { progress: None });
        }
        Ok(PollResult::Completed {
            artifacts: job.artifacts.clone(),
        })
    }

    async fn complete(&self, secret: &Secret, _remote_job: &RemoteJob) -> ProviderResult<()> {
        self.authenticate(secret)?;
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn cancel(
        &self,
        secret: &Secret,
        remote_job: &RemoteJob,
    ) -> ProviderResult<CancelResult> {
        self.authenticate(secret)?;
        let mut state = self.state.lock().expect("fake provider lock poisoned");
        let Some(job) = state.jobs.get_mut(&remote_job.id) else {
            return Ok(CancelResult::AlreadyTerminal);
        };
        if job.cancelled {
            return Ok(CancelResult::AlreadyTerminal);
        }
        job.cancelled = true;
        Ok(CancelResult::Cancelled)
    }
}
