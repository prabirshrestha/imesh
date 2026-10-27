FROM debian:trixie-slim
ENV container=docker
RUN apt-get update && apt-get install -y --no-install-recommends systemd systemd-sysv libpam-systemd dbus-user-session cron docker.io docker-cli openssh-server ca-certificates curl iproute2 procps sudo nftables socat \
 && rm -rf /var/lib/apt/lists/* \
 && systemctl enable ssh \
 && mkdir -p /root/.ssh && chmod 700 /root/.ssh \
 && useradd -m -s /bin/bash -G sudo deploy && echo 'deploy ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/deploy \
 && mkdir -p /home/deploy/.ssh && chmod 700 /home/deploy/.ssh && chown deploy:deploy /home/deploy/.ssh \
 && useradd -m -s /bin/bash ops \
 && mkdir -p /home/ops/.ssh && chmod 700 /home/ops/.ssh && chown ops:ops /home/ops/.ssh \
 && systemctl enable cron \
 && systemctl disable docker.service docker.socket containerd.service \
 && usermod -aG docker ops
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
