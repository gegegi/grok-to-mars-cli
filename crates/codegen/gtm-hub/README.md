# gtm-hub

Fork-owned Grok to Mars session hub. Not part of upstream `xai-grok-pager`.

`gtm hub` is intercepted in `xai-grok-pager-bin` **before** pager clap, so official grok-build merges do not fight a `Command::Hub` variant.

Each live session is an actor that spawns `gtm agent --no-leader stdio` (override with `GTM_HUB_AGENT_BIN`) and proxies ACP. `session/update` and permission requests fan out to subscribers.

LAN (opt-in, for a separate mobile client):

```
gtm remote --lan              # TLS 1.3 mTLS on :27420
gtm remote enroll [--ttl 30d] # ~/.gtm/gtm-hub.enroll (0600)
gtm remote list | revoke <id> | off | status
```

See `docs/hub.md` in the grok-to-mars umbrella repo.
