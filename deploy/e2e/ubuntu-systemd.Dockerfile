# A stand-in for a fresh Ubuntu 24.04 server: systemd as PID 1, nothing else
# beyond what cloud images ship (curl, openssl, ca-certificates).
FROM ubuntu:24.04
RUN apt-get update -qq \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
      systemd systemd-sysv dbus ca-certificates curl openssl iproute2 \
 && find /etc/systemd/system /lib/systemd/system -path '*.wants/*' \
      \( -name '*getty*' -o -name '*udev*' -o -name 'systemd-logind*' \) -delete
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
