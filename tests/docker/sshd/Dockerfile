# An OpenSSH server for the tunnel tests: password and public-key login for the user
# `treasury`, TCP forwarding allowed. Built by the tests through testcontainers.
FROM alpine:3.24.4
RUN apk add --no-cache openssh-server \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh treasury \
 && echo 'treasury:tunnel-password' | chpasswd \
 && install -d -m 700 -o treasury -g treasury /home/treasury/.ssh
COPY sshd_config /etc/ssh/sshd_config
COPY entrypoint.sh /entrypoint.sh
RUN chmod 755 /entrypoint.sh
EXPOSE 22
ENTRYPOINT ["/entrypoint.sh"]
