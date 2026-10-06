# A throwaway sandbox host for the ssh backend's live tests: sshd, a `henk`
# user whose key may only run henk-runner, and the sudoers rule. Build it with
# deploy/sandbox as the context and Henk's test public key:
#   podman build -f deploy/sandbox/test-host.Containerfile \
#     --build-arg HENK_PUBLIC_KEY="$(cat key.pub)" -t henk-sandbox deploy/sandbox
#   podman run -d --rm -p 127.0.0.1:2222:22 henk-sandbox
# Not for production: a real sandbox host is set up as the README says.
FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends openssh-server git sudo procps findutils tar ca-certificates curl xz-utils && rm -rf /var/lib/apt/lists/*
# mise for workspace profiles with toolchain = "mise" (#93), on the runner's PATH.
RUN curl -fsSL https://mise.run | MISE_INSTALL_PATH=/usr/local/bin/mise sh
RUN useradd --create-home --shell /bin/sh henk && mkdir -p /run/sshd /home/henk/.ssh
COPY henk-runner /usr/local/sbin/henk-runner
ARG HENK_PUBLIC_KEY
RUN test -n "$HENK_PUBLIC_KEY" && printf "%s\n" "$HENK_PUBLIC_KEY" > /tmp/henk_key.pub
RUN chmod 755 /usr/local/sbin/henk-runner \
 && printf 'restrict,command="/usr/local/sbin/henk-runner" %s\n' "$(cat /tmp/henk_key.pub)" > /home/henk/.ssh/authorized_keys \
 && chown -R henk:henk /home/henk/.ssh && chmod 700 /home/henk/.ssh && chmod 600 /home/henk/.ssh/authorized_keys \
 && echo 'henk ALL=(root) NOPASSWD: /usr/local/sbin/henk-runner' > /etc/sudoers.d/henk && chmod 440 /etc/sudoers.d/henk \
 && ssh-keygen -A
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
