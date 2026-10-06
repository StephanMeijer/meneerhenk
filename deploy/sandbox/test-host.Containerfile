# A throwaway sandbox host for the ssh backend's live tests: sshd, the tools
# Henk's sandbox script uses, mise, and Henk's test key for root. Nothing of
# Henk's is installed: the script comes with every request. Build it with
# Henk's test public key:
#   podman build -f deploy/sandbox/test-host.Containerfile \
#     --build-arg HENK_PUBLIC_KEY="$(cat key.pub)" -t henk-sandbox deploy/sandbox
#   podman run -d --rm -p 127.0.0.1:2222:22 henk-sandbox
# Not for production: a real sandbox host is set up as the README says.
FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends openssh-server git tar procps findutils grep passwd util-linux ca-certificates curl xz-utils && rm -rf /var/lib/apt/lists/*
# mise for workspace profiles with toolchain = "mise" (#93), on the run's PATH.
RUN curl -fsSL https://mise.run | MISE_INSTALL_PATH=/usr/local/bin/mise sh
ARG HENK_PUBLIC_KEY
RUN test -n "$HENK_PUBLIC_KEY" \
 && mkdir -p /run/sshd /root/.ssh && chmod 700 /root/.ssh \
 && printf "%s\n" "$HENK_PUBLIC_KEY" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys \
 && printf 'PermitRootLogin prohibit-password\nPasswordAuthentication no\n' > /etc/ssh/sshd_config.d/henk.conf \
 && ssh-keygen -A
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
