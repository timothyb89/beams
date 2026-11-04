# Agent protocol fixtures

`session_bind_*.b64`: real `session-bind@openssh.com` agent messages (base64
of the message body) captured from OpenSSH 10.0 logging in to a throwaway
local `sshd` restricted to one host key algorithm, via
`beam-bootstrap agent-proxy-spike` (its `session_bind` log's `raw` field).
They pin verification of host signatures made by OpenSSH itself, for key
types the unit tests can't easily sign with (`ssh-key` 0.6 can't produce
RSA signatures).
