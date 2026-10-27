//! Running the daemon in a Docker container on a server over SSH: `imesh add` and `imesh update server`
//! --docker`. For hosts where installing is hard, such as a NAS.
//!
//! The CLI builds the image on the server from the uploaded binary on `alpine:3`, which brings CA
//! certificates, so no registry holds imesh images. The container runs as a non-root user with a
//! read-only root file system, no capabilities, and host networking, so iroh sees the host's
//! addresses. Its key and database live in a named volume, so a new image keeps the machine.
//! Docker's restart policy starts it at boot, before anyone logs in.

use anyhow::{Context, Result, bail, ensure};
use iroh_mesh::client::Client;

use super::{
    Env, format_env, is_name, parse_env,
    remote::{Mode, Status, Target, quote},
};
use crate::{connect, ssh::Ssh};

const DOCKERFILE: &str = r#"FROM alpine:3
RUN mkdir /data && chown 65532:65532 /data
COPY imesh /imesh
USER 65532:65532
ENV HOME=/data IMESH_HOME=/data IMESH_SOCKET=/data/imesh.sock
VOLUME /data
ENTRYPOINT ["/imesh"]
CMD ["service", "run"]
"#;

/// Settings the image fixes; an update does not carry them over.
const FIXED: &[&str] = &["HOME", "IMESH_HOME", "IMESH_SOCKET", "PATH"];

/// The container options of `imesh add --docker` and `imesh update server --docker`.
#[derive(Debug, Clone, clap::Args)]
pub struct ContainerArgs {
    /// Container name. The volume with the key and database is `<name>-data`.
    #[arg(long, default_value = "imesh", requires = "docker")]
    pub docker_name: String,
    /// `host` gives iroh the host's addresses, for direct paths. `bridge` isolates the network
    /// more, and more traffic may go through a relay.
    #[arg(long, value_enum, default_value_t, requires = "docker")]
    pub docker_network: Network,
    /// Also mount this into the container, as for `docker run -v`, for example
    /// `/volume1/share:/share:ro`. Repeat for more.
    #[arg(long, requires = "docker")]
    pub docker_volume: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, clap::ValueEnum)]
pub enum Network {
    #[default]
    Host,
    Bridge,
}

/// How to run Docker on the server.
struct Docker {
    ssh: Ssh,
    /// `docker`, or `sudo -n docker`.
    cmd: &'static str,
}

impl Docker {
    async fn run(&self, args: &str) -> Result<String> {
        self.ssh.run(&format!("{} {args}", self.cmd), b"").await
    }

    async fn run_with(&self, args: &str, stdin: &[u8]) -> Result<String> {
        self.ssh.run(&format!("{} {args}", self.cmd), stdin).await
    }

    /// The container's settings, if it exists.
    async fn container_env(&self, name: &str) -> Option<Env> {
        let out = self
            .run(&format!("inspect -f '{{{{range .Config.Env}}}}{{{{println .}}}}{{{{end}}}}' {name}"))
            .await
            .ok()?;
        let mut env = parse_env(&out);
        env.retain(|k, _| !FIXED.contains(&k.as_str()));
        Some(env)
    }

    /// `imesh service status` in the container.
    async fn status(&self, name: &str) -> Result<Status> {
        let out = self.run(&format!("exec {name} /imesh service status")).await?;
        serde_json::from_str(out.trim()).with_context(|| format!("service status: {out}"))
    }
}

pub(super) async fn provision(target: &Target, mode: Mode) -> Result<Client> {
    let c = &target.container;
    ensure!(is_name(&c.docker_name), "bad container name {:?}", c.docker_name);
    let ssh = target.ssh_client();
    let probe = ssh
        .script(
            "uname -m\n\
             if docker version >/dev/null 2>&1; then echo docker; \
             elif sudo -n docker version >/dev/null 2>&1; then echo sudo; else echo none; fi",
        )
        .await?;
    let mut lines = probe.lines().map(str::trim);
    let arch = lines.next().unwrap_or_default().to_string();
    let docker = match lines.next() {
        Some("docker") => Docker { ssh, cmd: "docker" },
        Some("sudo") => Docker { ssh, cmd: "sudo -n docker" },
        _ => bail!(
            "the SSH user on {} cannot run docker: add it to the docker group, or allow it `sudo docker` with no password",
            target.display()
        ),
    };
    let name = &c.docker_name;
    let current = docker.container_env(name).await;
    let env = match mode {
        Mode::New | Mode::NewCluster => {
            if current.is_some()
                && let Ok(status) = docker.status(name).await
            {
                status.ensure_new(target, mode)?;
            }
            target.settings.to_env()
        }
        Mode::Update => {
            let mut env =
                current.with_context(|| format!("{} has no container {name}; use `imesh add`", target.display()))?;
            target.settings.net.clear_replaced_env(&mut env);
            env.extend(target.settings.to_env());
            env
        }
    };
    format_env(&env)?;

    let p = target.load_binary(crate::bundle::triple("linux", &arch)?)?;
    let (bin, bytes, sha) = (p.label, p.bytes, p.sha);
    let image = format!("imesh:{}", &sha[..12]);
    if docker.run(&format!("image inspect {image}")).await.is_err() {
        eprintln!("building {image} on {} from {} ({} MB)", target.display(), bin, bytes.len() >> 20);
        docker.run_with(&format!("build -q -t {image} -"), &context(&bytes)?).await.context("docker build")?;
    }

    let mut run = format!(
        "run -d --name {name} --restart unless-stopped --network {} -v {name}-data:/data \
         --read-only --tmpfs /tmp --cap-drop ALL --security-opt no-new-privileges",
        match c.docker_network {
            Network::Host => "host",
            Network::Bridge => "bridge",
        }
    );
    for v in &c.docker_volume {
        run.push_str(&format!(" -v {}", quote(v)));
    }
    for (k, v) in &env {
        run.push_str(&format!(" -e {}", quote(&format!("{k}={v}"))));
    }
    run.push_str(&format!(" {image}"));
    eprintln!("starting container {name} on {}", target.display());
    let _ = docker.run(&format!("rm -f {name}")).await;
    docker.run(&run).await.context("docker run")?;
    wait_until_running(&docker, name).await?;

    connect::open(&target.ssh())
        .await
        .with_context(|| format!("reach the imesh daemon in {name} on {}", target.display()))
}

async fn wait_until_running(docker: &Docker, name: &str) -> Result<()> {
    for _ in 0..60 {
        if docker.status(name).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let logs = docker.run(&format!("logs --tail 20 {name}")).await.unwrap_or_default();
    bail!("the daemon in container {name} did not start:\n{logs}")
}

/// The build context: the Dockerfile and the binary, as a tar stream for `docker build -`.
fn context(binary: &[u8]) -> Result<Vec<u8>> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut add = |path: &str, mode: u32, data: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_uid(0);
        h.set_gid(0);
        h.set_cksum();
        tar.append_data(&mut h, path, data)
    };
    add("Dockerfile", 0o644, DOCKERFILE.as_bytes())?;
    add("imesh", 0o755, binary)?;
    Ok(tar.into_inner()?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_context_holds_the_dockerfile_and_the_binary() {
        let tar = super::context(b"bin").unwrap();
        let mut archive = tar::Archive::new(tar.as_slice());
        let names: Vec<String> =
            archive.entries().unwrap().map(|e| e.unwrap().path().unwrap().display().to_string()).collect();
        assert_eq!(names, ["Dockerfile", "imesh"]);
    }
}
