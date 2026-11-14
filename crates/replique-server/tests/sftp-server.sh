#!/bin/sh
# A stand-in for a Storage Box login: password and key auth, SFTP, and an
# `install-ssh-key` command that adds keys read on stdin.
set -e
apk add --no-cache openssh-server openssh-sftp-server >/dev/null
ssh-keygen -A >/dev/null
adduser -D -s /bin/sh dev
echo "dev:DevPass-1234" | chpasswd
cat > /usr/local/bin/install-ssh-key <<'EOF'
#!/bin/sh
mkdir -p ~/.ssh && chmod 700 ~/.ssh && cat >> ~/.ssh/authorized_keys
EOF
chmod +x /usr/local/bin/install-ssh-key
exec /usr/sbin/sshd -D -e -o PasswordAuthentication=yes -o "Subsystem sftp internal-sftp"
