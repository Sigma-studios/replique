# A stand-in for a Storage Box sub-account, for the instrumented tests: SFTP
# only, the user chrooted to its home, which holds a writable /home, so the
# repository path is /home/restic as on Hetzner. Built locally because the
# common SFTP images ship for amd64 only.
FROM alpine:3.22

RUN apk add --no-cache openssh-server \
    && ssh-keygen -A \
    && adduser -D -u 1001 -h /home replique \
    # A locked account ("!") cannot log in even with a key; "*" just means no
    # password.
    && echo 'replique:*' | chpasswd -e \
    && mkdir -p /srv/sftp/replique/home /etc/ssh/keys \
    && chown replique:replique /srv/sftp/replique/home

# StrictModes is off because the bind-mounted key belongs to whoever runs Docker.
RUN printf '%s\n' \
    'Port 22' \
    'PasswordAuthentication no' \
    'KbdInteractiveAuthentication no' \
    'StrictModes no' \
    'AuthorizedKeysFile /etc/ssh/keys/%u.pub' \
    'Subsystem sftp internal-sftp' \
    'Match User replique' \
    '    ChrootDirectory /srv/sftp/replique' \
    '    ForceCommand internal-sftp' \
    '    AllowTcpForwarding no' \
    > /etc/ssh/sshd_config

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
