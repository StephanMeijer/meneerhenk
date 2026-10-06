//! The kubernetes backend (#89): a workspace is a Pod of its own in a
//! sandbox namespace, apart from Henk's.
//!
//! Henk talks to the API server himself, with his own service account (or a
//! kubeconfig outside the cluster), and needs nothing but Pods and
//! `pods/exec` in that one namespace. Each Pod runs one idle process; every
//! request is a `pods/exec` of Henk's sandbox script in its single-user
//! mode, the same script and the same workspace (`remote.rs`) as the ssh
//! backend. Within the Pod everything, the record of changes included, is
//! the run's own user; the Pod is the boundary, and what leaves it is the
//! changeset Henk checks as for every backend.
//!
//! The Pod is locked down by its spec: not root, no privilege escalation,
//! no capabilities, a read-only root filesystem, seccomp, no service account
//! token and no service links, memory, cpu and scratch disk from the
//! profile's limits. The number of processes is the node's setting. What it
//! may reach on the network is the namespace's `NetworkPolicy`
//! (`deploy/kubernetes/`).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use henk_domain::workspace::{Limits, Profile};
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, EnvVar, Pod, PodSecurityContext, PodSpec,
    ResourceRequirements, SeccompProfile, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Status};
use kube::api::{Api, AttachParams, DeleteParams, ListParams, PostParams};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::remote::{
    Kept, REQUEST_WAIT, RemoteWorkspace, Reply, Runner, SCRIPT, check_tokens, new_workspace_id,
    pack,
};
use super::{Sweep, Workspace, WorkspaceError, WorkspaceProvider};

/// The label every sandbox Pod carries, and its value: what a sweep removes.
const MANAGED_BY: (&str, &str) = ("app.kubernetes.io/managed-by", "meneer-henk");
/// The label that names a Pod's workspace.
const WORKSPACE: &str = "henk.workspace";
/// The one container of a sandbox Pod.
const CONTAINER: &str = "work";
/// Where the workspaces live in the Pod, a scratch volume.
const SANDBOX: &str = "/sandbox";
/// How long a Pod may take to start: scheduling and pulling the image.
const POD_WAIT: Duration = Duration::from_mins(5);
/// How long a sandbox Pod lives at most, whatever happens to Henk: longer
/// than any run, so the cluster removes one Henk lost track of, and the
/// sweep when Henk starts removes it sooner.
const POD_DEADLINE_SECS: i64 = 6 * 60 * 60;

/// How the Pods are made, from `[workspace.kubernetes]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodSettings {
    /// The sandbox namespace.
    pub namespace: String,
    /// The image when a profile names none.
    pub image: Option<String>,
    /// The user the Pod runs as; never root.
    pub run_as_user: i64,
    /// A runtime class, such as gVisor or Kata.
    pub runtime_class: Option<String>,
    /// Node labels the Pods must land on.
    pub node_selector: BTreeMap<String, String>,
}

/// `mib` mebibytes as a quantity.
fn mebibytes(mib: u64) -> Quantity {
    Quantity(format!("{mib}Mi"))
}

/// The Pod of workspace `id`: one idle container from `image`, locked down,
/// with the profile's limits.
#[must_use]
pub fn pod(settings: &PodSettings, id: &str, image: &str, limits: &Limits) -> Pod {
    let labels = BTreeMap::from([
        (MANAGED_BY.0.to_owned(), MANAGED_BY.1.to_owned()),
        (WORKSPACE.to_owned(), id.to_owned()),
    ]);
    let mut bounds = BTreeMap::new();
    if let Some(mib) = limits.memory_mib {
        bounds.insert("memory".to_owned(), mebibytes(mib));
    }
    if let Some(cpus) = limits.cpus {
        bounds.insert("cpu".to_owned(), Quantity(cpus.to_string()));
    }
    if let Some(mib) = limits.disk_mib {
        bounds.insert("ephemeral-storage".to_owned(), mebibytes(mib));
    }
    let scratch = |name: &str| Volume {
        name: name.to_owned(),
        empty_dir: Some(EmptyDirVolumeSource {
            size_limit: limits.disk_mib.map(mebibytes),
            ..EmptyDirVolumeSource::default()
        }),
        ..Volume::default()
    };
    let mount = |name: &str, path: &str| VolumeMount {
        name: name.to_owned(),
        mount_path: path.to_owned(),
        ..VolumeMount::default()
    };
    let env = |name: &str, value: &str| EnvVar {
        name: name.to_owned(),
        value: Some(value.to_owned()),
        ..EnvVar::default()
    };
    Pod {
        metadata: ObjectMeta {
            name: Some(format!("henk-{id}")),
            namespace: Some(settings.namespace.clone()),
            labels: Some(labels),
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: CONTAINER.to_owned(),
                image: Some(image.to_owned()),
                command: Some(vec!["sleep".to_owned(), "infinity".to_owned()]),
                working_dir: Some(SANDBOX.to_owned()),
                env: Some(vec![
                    env("HENK_SANDBOX_BASE", SANDBOX),
                    env("HENK_SANDBOX_POD", "1"),
                ]),
                resources: (!bounds.is_empty()).then(|| ResourceRequirements {
                    limits: Some(bounds.clone()),
                    requests: Some(bounds),
                    ..ResourceRequirements::default()
                }),
                security_context: Some(SecurityContext {
                    allow_privilege_escalation: Some(false),
                    read_only_root_filesystem: Some(true),
                    run_as_non_root: Some(true),
                    capabilities: Some(Capabilities {
                        drop: Some(vec!["ALL".to_owned()]),
                        ..Capabilities::default()
                    }),
                    ..SecurityContext::default()
                }),
                volume_mounts: Some(vec![mount("sandbox", SANDBOX), mount("tmp", "/tmp")]),
                ..Container::default()
            }],
            volumes: Some(vec![scratch("sandbox"), scratch("tmp")]),
            security_context: Some(PodSecurityContext {
                run_as_non_root: Some(true),
                run_as_user: Some(settings.run_as_user),
                run_as_group: Some(settings.run_as_user),
                fs_group: Some(settings.run_as_user),
                seccomp_profile: Some(SeccompProfile {
                    type_: "RuntimeDefault".to_owned(),
                    ..SeccompProfile::default()
                }),
                ..PodSecurityContext::default()
            }),
            restart_policy: Some("Never".to_owned()),
            automount_service_account_token: Some(false),
            enable_service_links: Some(false),
            active_deadline_seconds: Some(POD_DEADLINE_SECS),
            termination_grace_period_seconds: Some(0),
            runtime_class_name: settings.runtime_class.clone(),
            node_selector: (!settings.node_selector.is_empty())
                .then(|| settings.node_selector.clone()),
            ..PodSpec::default()
        }),
        ..Pod::default()
    }
}

/// The exit code `pods/exec` reports: 0 on success, the code of a
/// `NonZeroExitCode` failure, and none when the command did not end by
/// itself.
fn exit_code(status: Option<&Status>) -> Option<i32> {
    let status = status?;
    if status.status.as_deref() == Some("Success") {
        return Some(0);
    }
    status
        .details
        .as_ref()?
        .causes
        .as_ref()?
        .iter()
        .find(|cause| cause.reason.as_deref() == Some("ExitCode"))?
        .message
        .as_deref()?
        .parse()
        .ok()
}

fn api_error(what: &str) -> impl Fn(kube::Error) -> WorkspaceError + '_ {
    move |error| WorkspaceError::Backend(format!("{what} failed: {error}"))
}

/// Requests to one workspace's Pod, through `pods/exec`.
pub(crate) struct PodRunner {
    pods: Api<Pod>,
    name: String,
}

impl PodRunner {
    /// Why the workspace's container is no longer running, when it is not:
    /// on Kubernetes 1.32 and later, a command that runs out of memory ends
    /// the whole container, and with it the workspace.
    async fn ended(&self) -> Option<String> {
        let pod = self.pods.get(&self.name).await.ok()?;
        let state = pod
            .status?
            .container_statuses?
            .into_iter()
            .find(|c| c.name == CONTAINER)?
            .state?;
        let terminated = state.terminated?;
        Some(match terminated.reason.as_deref() {
            Some("OOMKilled") => {
                "the workspace ran out of its memory limit (OOMKilled) and was stopped".to_owned()
            }
            Some(reason) => format!("the workspace was stopped ({reason})"),
            None => format!(
                "the workspace was stopped (exit code {})",
                terminated.exit_code
            ),
        })
    }

    /// [`Self::ended`], after waiting a little for the Pod's status: the
    /// kubelet reports a container it killed a moment after the kill.
    async fn ended_by_now(&self) -> Option<String> {
        for _ in 0..10 {
            if let Some(why) = self.ended().await {
                return Some(why);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        None
    }

    /// Deletes the Pod at once; one that is gone already is fine.
    async fn delete(&self) -> Result<(), WorkspaceError> {
        match self
            .pods
            .delete(
                &self.name,
                &DeleteParams {
                    grace_period_seconds: Some(0),
                    ..DeleteParams::default()
                },
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(()),
            Err(error) => Err(api_error("deleting the sandbox Pod")(error)),
        }
    }

    /// Waits until the Pod runs, or says why it will not.
    async fn running(&self) -> Result<(), WorkspaceError> {
        let started = tokio::time::Instant::now();
        loop {
            let pod = self
                .pods
                .get(&self.name)
                .await
                .map_err(api_error("reading the sandbox Pod"))?;
            let status = pod.status.unwrap_or_default();
            let waiting = status
                .container_statuses
                .unwrap_or_default()
                .into_iter()
                .find_map(|c| c.state.and_then(|s| s.waiting));
            match status.phase.as_deref() {
                Some("Running") => return Ok(()),
                Some(phase @ ("Failed" | "Succeeded")) => {
                    return Err(WorkspaceError::Backend(format!(
                        "the sandbox Pod {} ended ({phase}) before it was used",
                        self.name
                    )));
                }
                _ => {}
            }
            if let Some(waiting) = &waiting
                && let Some(
                    reason @ ("ErrImagePull"
                    | "ImagePullBackOff"
                    | "InvalidImageName"
                    | "CreateContainerConfigError"
                    | "CreateContainerError"),
                ) = waiting.reason.as_deref()
            {
                return Err(WorkspaceError::Backend(format!(
                    "the sandbox Pod cannot start: {reason}: {}",
                    waiting.message.as_deref().unwrap_or("no message")
                )));
            }
            if started.elapsed() >= POD_WAIT {
                let why = status
                    .conditions
                    .unwrap_or_default()
                    .into_iter()
                    .find(|c| c.status == "False")
                    .and_then(|c| c.message)
                    .unwrap_or_else(|| "it is still pending".to_owned());
                return Err(WorkspaceError::Backend(format!(
                    "the sandbox Pod did not start within {}s: {why}",
                    POD_WAIT.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Reads `stream` to its end, keeping what `keep` says.
async fn drain(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    keep: Option<usize>,
) -> Result<Vec<u8>, WorkspaceError> {
    let mut kept = Kept::new(keep);
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| WorkspaceError::Backend(format!("reading from the sandbox Pod: {e}")))?;
        if n == 0 {
            return Ok(kept.finish());
        }
        kept.push(chunk.get(..n).unwrap_or_default())?;
    }
}

#[async_trait::async_trait]
impl Runner for PodRunner {
    async fn call(
        &self,
        tokens: &[String],
        stdin: &[u8],
        keep: Option<usize>,
        wait: Duration,
    ) -> Result<Reply, WorkspaceError> {
        check_tokens(tokens)?;
        // Each word is its own argument of the exec: nothing is parsed by a
        // shell on the way, and the script checks the tokens again.
        let command: Vec<String> = ["sh", "-c", SCRIPT, "henk-sandbox"]
            .into_iter()
            .map(str::to_owned)
            .chain(tokens.iter().cloned())
            .collect();
        let params = AttachParams::default()
            .container(CONTAINER)
            .stdin(true)
            .stdout(true)
            .stderr(true);
        let exchange = async {
            let mut process = self
                .pods
                .exec(&self.name, command, &params)
                .await
                .map_err(api_error("running a request in the sandbox Pod"))?;
            let status = process.take_status();
            let (Some(mut input), Some(mut out), Some(mut err)) =
                (process.stdin(), process.stdout(), process.stderr())
            else {
                return Err(WorkspaceError::Backend(
                    "the sandbox Pod gave no streams".to_owned(),
                ));
            };
            let feed = async {
                input.write_all(stdin).await?;
                input.shutdown().await?;
                drop(input);
                Ok::<(), std::io::Error>(())
            };
            let (sent, stdout, stderr) =
                tokio::join!(feed, drain(&mut out, keep), drain(&mut err, keep));
            sent.map_err(|e| WorkspaceError::Backend(format!("writing to the sandbox Pod: {e}")))?;
            let code = match status {
                Some(status) => exit_code(status.await.as_ref()),
                None => None,
            };
            let _ = process.join().await;
            Ok(Reply {
                code,
                stdout: stdout?,
                stderr: stderr?,
                gave_up: false,
            })
        };
        let reply = match tokio::time::timeout(wait, exchange).await {
            Ok(reply) => reply,
            Err(_) => Ok(Reply {
                gave_up: true,
                ..Reply::default()
            }),
        };
        // A request that did not end by itself, or could not start, may
        // have lost its workspace: then that is what it says.
        match reply {
            Ok(reply) if reply.code == Some(0) => Ok(reply),
            Ok(mut reply) => {
                // An ordinary failure (grep found nothing, a test failed) is
                // looked at once; a kill is waited on.
                let killed = matches!(reply.code, None | Some(137));
                let why = if killed {
                    self.ended_by_now().await
                } else {
                    self.ended().await
                };
                if let Some(why) = why {
                    if !reply.stderr.is_empty() && !reply.stderr.ends_with(b"\n") {
                        reply.stderr.push(b'\n');
                    }
                    reply.stderr.extend_from_slice(why.as_bytes());
                }
                Ok(reply)
            }
            Err(error) => Err(match self.ended_by_now().await {
                Some(why) => WorkspaceError::Backend(why),
                None => error,
            }),
        }
    }

    /// The workspace is the Pod: removing it removes everything.
    async fn release(&self, _id: &str) -> Result<(), WorkspaceError> {
        self.delete().await
    }
}

/// Workspaces as Pods in the sandbox namespace.
#[derive(Clone)]
pub struct KubernetesProvider {
    client: kube::Client,
    settings: PodSettings,
    /// Held to write by a sweep and to read by an open, so a sweep never
    /// removes a Pod being made.
    gate: Arc<tokio::sync::RwLock<()>>,
}

impl std::fmt::Debug for KubernetesProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KubernetesProvider")
            .field("namespace", &self.settings.namespace)
            .finish_non_exhaustive()
    }
}

impl KubernetesProvider {
    /// A provider that makes Pods as `settings` says through `client`.
    #[must_use]
    pub fn new(client: kube::Client, settings: PodSettings) -> Self {
        Self {
            client,
            settings,
            gate: Arc::new(tokio::sync::RwLock::new(())),
        }
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.settings.namespace)
    }

    fn selector() -> String {
        format!("{}={}", MANAGED_BY.0, MANAGED_BY.1)
    }

    /// Removes every sandbox Pod in the namespace: what a process that died
    /// left. One Henk per sandbox namespace, and only when it starts; with
    /// workspaces being opened already, the sweep is refused.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the API cannot be reached or a
    /// workspace is being opened.
    pub fn sweep(&self) -> impl Future<Output = Result<(), WorkspaceError>> + Send + 'static {
        let gate = Arc::clone(&self.gate).try_write_owned();
        let pods = self.pods();
        async move {
            let _gate = gate.map_err(|_| {
                WorkspaceError::Backend(
                    "a workspace is being opened; the sandbox namespace is not swept".to_owned(),
                )
            })?;
            let found = pods
                .list(&ListParams::default().labels(&Self::selector()))
                .await
                .map_err(api_error("listing the sandbox Pods"))?;
            for pod in found {
                if let Some(name) = pod.metadata.name {
                    PodRunner {
                        pods: pods.clone(),
                        name,
                    }
                    .delete()
                    .await?;
                }
            }
            Ok(())
        }
    }

    /// What Henk may do in the sandbox namespace, as the API server says:
    /// each line `verb resource: allowed` or `: refused`. Henk needs Pods
    /// and `pods/exec` there and must not read Secrets.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the API cannot be reached.
    pub async fn access(&self) -> Result<Vec<(String, bool)>, WorkspaceError> {
        let reviews: Api<SelfSubjectAccessReview> = Api::all(self.client.clone());
        let mut answers = Vec::new();
        for (verb, resource, subresource) in [
            ("create", "pods", None),
            ("get", "pods", None),
            ("list", "pods", None),
            ("delete", "pods", None),
            ("create", "pods", Some("exec")),
            ("get", "secrets", None),
        ] {
            let review = SelfSubjectAccessReview {
                spec: SelfSubjectAccessReviewSpec {
                    resource_attributes: Some(ResourceAttributes {
                        namespace: Some(self.settings.namespace.clone()),
                        verb: Some(verb.to_owned()),
                        resource: Some(resource.to_owned()),
                        subresource: subresource.map(str::to_owned),
                        ..ResourceAttributes::default()
                    }),
                    ..SelfSubjectAccessReviewSpec::default()
                },
                ..SelfSubjectAccessReview::default()
            };
            let answer = reviews
                .create(&PostParams::default(), &review)
                .await
                .map_err(api_error("asking the API server what Henk may do"))?;
            let shown = match subresource {
                Some(sub) => format!("{verb} {resource}/{sub}"),
                None => format!("{verb} {resource}"),
            };
            answers.push((shown, answer.status.is_some_and(|s| s.allowed)));
        }
        Ok(answers)
    }

    /// Starts a Pod from `image`, runs the script's `probe` in it and
    /// removes it: the tools the image has, by the script's own report.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the Pod cannot be made or started.
    pub async fn probe(&self, image: &str) -> Result<String, WorkspaceError> {
        let runner = self.start(image, &Limits::default()).await?;
        let reply = runner
            .call(&["probe".to_owned()], &[], None, REQUEST_WAIT)
            .await
            .and_then(|reply| reply.ok("probing the sandbox image"));
        let _ = runner.delete().await;
        Ok(String::from_utf8_lossy(&reply?.stdout).into_owned())
    }

    /// Makes a Pod and waits until it runs; one that does not is removed.
    async fn start(&self, image: &str, limits: &Limits) -> Result<PodRunner, WorkspaceError> {
        let id = new_workspace_id();
        let spec = pod(&self.settings, &id, image, limits);
        let runner = PodRunner {
            pods: self.pods(),
            name: format!("henk-{id}"),
        };
        runner
            .pods
            .create(&PostParams::default(), &spec)
            .await
            .map_err(api_error("making the sandbox Pod"))?;
        if let Err(error) = runner.running().await {
            let _ = runner.delete().await;
            return Err(error);
        }
        Ok(runner)
    }
}

#[async_trait::async_trait]
impl WorkspaceProvider for KubernetesProvider {
    async fn open(
        &self,
        source: &Path,
        profile: &Profile,
    ) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        let image = profile
            .image
            .clone()
            .or_else(|| self.settings.image.clone())
            .ok_or_else(|| {
                WorkspaceError::Backend(
                    "the kubernetes backend has no image for this profile".to_owned(),
                )
            })?;
        let source = source.to_owned();
        let archive = tokio::task::spawn_blocking(move || pack(&source))
            .await
            .map_err(|e| WorkspaceError::Backend(format!("cannot pack the checkout: {e}")))??;
        // A sweep in progress ends first.
        let _swept = self.gate.read().await;
        let runner = self.start(&image, &profile.limits).await?;
        let id = runner
            .name
            .strip_prefix("henk-")
            .unwrap_or_default()
            .to_owned();
        // From here the workspace owns the Pod: a failed import drops it,
        // which deletes the Pod.
        let workspace = RemoteWorkspace::start(Arc::new(runner), id, profile, &archive).await?;
        Ok(Arc::new(workspace))
    }

    fn sweep(&self) -> Option<Sweep> {
        Some(Box::pin(self.sweep()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{StatusCause, StatusDetails};

    use super::*;

    fn settings() -> PodSettings {
        PodSettings {
            namespace: "henk-sandbox".to_owned(),
            image: Some("sandbox:1".to_owned()),
            run_as_user: 1000,
            runtime_class: Some("gvisor".to_owned()),
            node_selector: BTreeMap::from([("pool".to_owned(), "sandbox".to_owned())]),
        }
    }

    #[test]
    fn a_pod_is_locked_down_and_bounded_by_the_profile() {
        let limits = Limits {
            memory_mib: Some(2048),
            cpus: Some(2),
            disk_mib: Some(4096),
            ..Limits::default()
        };
        let pod = pod(&settings(), "w0123456789ab", "sandbox:2", &limits);
        let meta = &pod.metadata;
        assert_eq!(meta.name.as_deref(), Some("henk-w0123456789ab"));
        assert_eq!(meta.namespace.as_deref(), Some("henk-sandbox"));
        let labels = meta.labels.as_ref().unwrap();
        assert_eq!(labels["app.kubernetes.io/managed-by"], "meneer-henk");
        assert_eq!(labels["henk.workspace"], "w0123456789ab");

        let spec = pod.spec.as_ref().unwrap();
        assert_eq!(spec.automount_service_account_token, Some(false));
        assert_eq!(spec.enable_service_links, Some(false));
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
        assert_eq!(spec.runtime_class_name.as_deref(), Some("gvisor"));
        assert_eq!(spec.node_selector.as_ref().unwrap()["pool"], "sandbox");
        assert!(spec.active_deadline_seconds.is_some());
        assert!(
            spec.service_account_name.is_none(),
            "the namespace's default, without a token"
        );
        let pod_security = spec.security_context.as_ref().unwrap();
        assert_eq!(pod_security.run_as_non_root, Some(true));
        assert_eq!(pod_security.run_as_user, Some(1000));
        assert_eq!(
            pod_security.seccomp_profile.as_ref().unwrap().type_,
            "RuntimeDefault"
        );

        let [container] = spec.containers.as_slice() else {
            panic!("one container")
        };
        assert_eq!(container.image.as_deref(), Some("sandbox:2"));
        let security = container.security_context.as_ref().unwrap();
        assert_eq!(security.allow_privilege_escalation, Some(false));
        assert_eq!(security.read_only_root_filesystem, Some(true));
        assert_eq!(
            security.capabilities.as_ref().unwrap().drop.as_deref(),
            Some(["ALL".to_owned()].as_slice())
        );
        let bounds = container
            .resources
            .as_ref()
            .unwrap()
            .limits
            .as_ref()
            .unwrap();
        assert_eq!(bounds["memory"], Quantity("2048Mi".to_owned()));
        assert_eq!(bounds["cpu"], Quantity("2".to_owned()));
        assert_eq!(bounds["ephemeral-storage"], Quantity("4096Mi".to_owned()));
        let env: Vec<(&str, &str)> = container
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap()))
            .collect();
        assert_eq!(
            env,
            [("HENK_SANDBOX_BASE", "/sandbox"), ("HENK_SANDBOX_POD", "1")]
        );
        let volumes = spec.volumes.as_ref().unwrap();
        assert!(volumes.iter().all(|v| {
            v.empty_dir.as_ref().unwrap().size_limit == Some(Quantity("4096Mi".to_owned()))
        }));
    }

    #[test]
    fn without_limits_a_pod_asks_for_nothing() {
        let mut plain = settings();
        plain.runtime_class = None;
        plain.node_selector.clear();
        let pod = pod(&plain, "w0123456789ab", "sandbox:1", &Limits::default());
        let spec = pod.spec.unwrap();
        assert!(spec.containers[0].resources.is_none());
        assert!(spec.runtime_class_name.is_none());
        assert!(spec.node_selector.is_none());
    }

    /// The cluster of the live tests: the current kubeconfig context,
    /// acting as the service account `HENK_TEST_KUBE_SERVICEACCOUNT`
    /// (`namespace:name`, Henk's own, bound by `deploy/kubernetes/`), the
    /// namespace `HENK_TEST_KUBE_NAMESPACE` and the image
    /// `HENK_TEST_KUBE_IMAGE` (`deploy/kubernetes/sandbox-image`).
    pub(crate) async fn live_provider() -> KubernetesProvider {
        live_provider_in(&live_var("NAMESPACE")).await
    }

    fn live_var(name: &str) -> String {
        std::env::var(format!("HENK_TEST_KUBE_{name}"))
            .unwrap_or_else(|_| panic!("HENK_TEST_KUBE_{name} is not set"))
    }

    /// [`live_provider`] in `namespace`.
    pub(crate) async fn live_provider_in(namespace: &str) -> KubernetesProvider {
        let mut config = kube::Config::infer().await.unwrap();
        config.auth_info.impersonate = Some(format!(
            "system:serviceaccount:{}",
            live_var("SERVICEACCOUNT")
        ));
        let client = kube::Client::try_from(config).unwrap();
        KubernetesProvider::new(
            client,
            PodSettings {
                namespace: namespace.to_owned(),
                image: Some(live_var("IMAGE")),
                run_as_user: 1000,
                runtime_class: None,
                node_selector: BTreeMap::new(),
            },
        )
    }

    /// The sandbox Pods left in the live tests' namespace.
    pub(crate) async fn pods_left(provider: &KubernetesProvider) -> Vec<String> {
        provider
            .pods()
            .list(&ListParams::default().labels(&KubernetesProvider::selector()))
            .await
            .unwrap()
            .into_iter()
            .filter(|p| p.metadata.deletion_timestamp.is_none())
            .filter_map(|p| p.metadata.name)
            .collect()
    }

    /// The live tests share one namespace and check that it is empty after
    /// each, so they run one at a time.
    pub(crate) static LIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    #[ignore = "needs a cluster (HENK_TEST_KUBE_*)"]
    async fn live_kube_the_backend_keeps_the_workspace_contract() {
        let _one = LIVE.lock().await;
        let provider = live_provider().await;
        crate::workspace::contract::every_backend_does_this(
            Arc::new(provider.clone()),
            "henk-kube-live",
        )
        .await;
        assert_eq!(pods_left(&provider).await, Vec::<String>::new());
    }

    #[tokio::test]
    #[ignore = "needs a cluster (HENK_TEST_KUBE_*)"]
    async fn live_kube_henk_may_do_exactly_its_work_and_only_in_the_sandbox() {
        let _one = LIVE.lock().await;
        let provider = live_provider().await;
        let access = provider.access().await.unwrap();
        for (what, allowed) in &access {
            assert_eq!(
                *allowed,
                what != "get secrets",
                "{what} in the sandbox namespace"
            );
        }
        let elsewhere = live_provider_in("default").await;
        assert!(
            elsewhere
                .access()
                .await
                .unwrap()
                .iter()
                .all(|(_, allowed)| !allowed),
            "nothing in another namespace"
        );
        let report = provider.probe(&live_var("IMAGE")).await.unwrap();
        assert!(report.starts_with("henk-sandbox "), "{report}");
        assert!(!report.contains(" missing"), "{report}");
        assert_eq!(pods_left(&provider).await, Vec::<String>::new());
    }

    #[tokio::test]
    #[ignore = "needs a cluster (HENK_TEST_KUBE_*)"]
    async fn live_kube_export_stops_what_the_run_left_running() {
        use henk_domain::address::WorkspacePath;
        let _one = LIVE.lock().await;
        let provider = live_provider().await;
        let source = crate::workspace::contract::source("henk-kube-live-stop-src");
        let ws = provider
            .open(source.path(), &Profile::default())
            .await
            .unwrap();
        let sh = |script: &str| vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()];
        // A writer left running in the background, as a model's command can.
        let left = ws
            .exec(
                &sh("(while :; do date +%N > tick; sleep 0.1; done >/dev/null 2>&1 &); sleep 0.5"),
                &WorkspacePath::root(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(left.code, Some(0), "{}", left.output);
        let first = ws.export().await.unwrap();
        let second = ws.export().await.unwrap();
        assert_eq!(
            first, second,
            "nothing changes the tree once export has stopped the run"
        );
        // Only the Pod's own first process is left.
        let others = ws
            .exec(
                &sh("for p in /proc/[0-9]*; do [ \"${p#/proc/}\" = 1 ] || [ \"${p#/proc/}\" = $$ ] || cat \"$p/comm\" 2>/dev/null; done | grep -cx sleep"),
                &WorkspacePath::root(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(
            others.output.trim(),
            "0",
            "export stopped what the run left behind"
        );
        ws.close().await;
        assert_eq!(pods_left(&provider).await, Vec::<String>::new());
    }

    #[tokio::test]
    #[ignore = "needs a cluster (HENK_TEST_KUBE_*)"]
    async fn live_kube_a_command_past_its_memory_is_killed() {
        let _one = LIVE.lock().await;
        let provider = live_provider().await;
        let dir = crate::git::ScratchDir::new("henk-kube-memory").unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        let profile = Profile {
            limits: Limits {
                memory_mib: Some(64),
                ..Limits::default()
            },
            ..Profile::default()
        };
        let ws = provider.open(dir.path(), &profile).await.unwrap();
        let argv = [
            "sh".to_owned(),
            "-c".to_owned(),
            "x=$(head -c 300000000 /dev/zero | tr '\\0' a); echo survived".to_owned(),
        ];
        let result = ws
            .exec(
                &argv,
                &henk_domain::address::WorkspacePath::root(),
                Duration::from_mins(1),
            )
            .await
            .unwrap();
        assert_ne!(result.code, Some(0), "{result:?}");
        assert!(!result.output.contains("survived"), "{result:?}");
        assert!(
            result
                .output
                .contains("ran out of its memory limit (OOMKilled)"),
            "the result says why: {result:?}"
        );
        // On Kubernetes 1.32 and later the whole container ends with the
        // command, and every later request says why.
        let after = ws
            .exec(
                &["true".to_owned()],
                &henk_domain::address::WorkspacePath::root(),
                Duration::from_secs(10),
            )
            .await;
        match after {
            Ok(done) if done.code == Some(0) => {}
            Ok(done) => assert!(done.output.contains("memory"), "{done:?}"),
            Err(error) => assert!(error.to_string().contains("OOMKilled"), "{error}"),
        }
        ws.close().await;
        assert_eq!(pods_left(&provider).await, Vec::<String>::new());
    }

    #[test]
    fn the_exit_code_comes_from_the_exec_status() {
        let success = Status {
            status: Some("Success".to_owned()),
            ..Status::default()
        };
        assert_eq!(exit_code(Some(&success)), Some(0));
        let failed = Status {
            status: Some("Failure".to_owned()),
            reason: Some("NonZeroExitCode".to_owned()),
            details: Some(StatusDetails {
                causes: Some(vec![StatusCause {
                    reason: Some("ExitCode".to_owned()),
                    message: Some("3".to_owned()),
                    ..StatusCause::default()
                }]),
                ..StatusDetails::default()
            }),
            ..Status::default()
        };
        assert_eq!(exit_code(Some(&failed)), Some(3));
        assert_eq!(exit_code(None), None);
        assert_eq!(
            exit_code(Some(&Status {
                status: Some("Failure".to_owned()),
                ..Status::default()
            })),
            None
        );
    }
}
