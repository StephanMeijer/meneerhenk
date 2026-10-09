# A throwaway sandbox host for the ssh backend's live tests: sshd, the tools
# Henk's sandbox script uses, mise, and Henk's test key for root. Nothing of
# Henk's is installed: the script comes with every request. The key line
# starts with `restrict`, as the README advises, so the live tests show that
# Henk needs no pty or forwarding; `from=` is left out, since the address a
# container sees depends on the container network. Build it with
# Henk's test public key:
#   podman build -f deploy/sandbox/test-host.Containerfile \
#     --build-arg HENK_PUBLIC_KEY="$(cat key.pub)" -t henk-sandbox deploy/sandbox
#   podman run -d --rm -p 127.0.0.1:2222:22 henk-sandbox
# Not for production: a real sandbox host is set up as the README says.
FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends openssh-server git tar procps findutils grep passwd util-linux ca-certificates curl xz-utils && rm -rf /var/lib/apt/lists/*
# mise for workspace profiles with toolchain = "mise" (#93), on the run's PATH.
# A fixed mise release, checked against the SHA256 kept here, so every
# build installs the same binary. To update it, take the new release's
# lines from its SHASUMS256.txt, and keep it the same release as
# deploy/kubernetes/sandbox-image/Containerfile (#182).
ARG MISE_VERSION=v2026.10.3
ARG MISE_SHA256_X64=8d48bc510b7d844fad0bc7156c0855c2a1e7230c51e498a7f6b63b021f8b57c5
ARG MISE_SHA256_ARM64=357260e28904569a6e7124d33b65cef6d043846c07bb8f4906ddf22cc353d61d
# Set by the builder (BuildKit, podman); a build without it fails here.
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) arch=x64; sum=$MISE_SHA256_X64 ;; \
      arm64) arch=arm64; sum=$MISE_SHA256_ARM64 ;; \
      *) echo "no mise checksum for architecture '$TARGETARCH'" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /tmp/mise "https://github.com/jdx/mise/releases/download/$MISE_VERSION/mise-$MISE_VERSION-linux-$arch" \
 && echo "$sum  /tmp/mise" | sha256sum -c - \
 && install -m 0755 /tmp/mise /usr/local/bin/mise \
 && rm /tmp/mise
ARG HENK_PUBLIC_KEY
RUN test -n "$HENK_PUBLIC_KEY" \
 && mkdir -p /run/sshd /root/.ssh && chmod 700 /root/.ssh \
 && printf "restrict %s\n" "$HENK_PUBLIC_KEY" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys \
 && printf 'PermitRootLogin prohibit-password\nPasswordAuthentication no\n' > /etc/ssh/sshd_config.d/henk.conf \
 && ssh-keygen -A
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
