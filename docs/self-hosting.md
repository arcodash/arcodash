# Self-hosting

`docker-compose.prod.yml` runs arcodash on a single server: Postgres, Valkey and the bundled
object store on named volumes, a one-shot `migrate` job, then the backend, worker and web app,
all with `restart: unless-stopped`. Only the web app and the backend API are published.

You need Docker with the Compose plugin, a hostname or IP for the server, and an SMTP server for
invitations, password resets and alert emails.

## 1. Configure

```bash
cp .env.selfhost.example .env
```

Replace every `CHANGE-ME` and fill in the blank values in `.env`:

- `POSTGRES_PASSWORD`, and the same password inside `DATABASE_URL`.
- `COOKIE_SECRET` and `ENCRYPTION_KEY` (blank in the template; the backend refuses to start
  without them): run `openssl rand -hex 32` for each. `ENCRYPTION_KEY`
  encrypts data source credentials and destination secrets, so back it up; without it they
  cannot be decrypted. To change it later, see
  [`ENCRYPTION_KEY` rotation](../ENV_VARS.md#security-notes).
- `BASE_URL`: the address users open, for example `https://arcodash.example.com`. It is the only
  allowed CORS origin and the base of links in emails.
- `API_BASE_URL`: where the browser reaches the backend. It is built into the frontend image, so
  run `up --build` again after changing it.
- `ADMIN_EMAIL` and `ADMIN_PASSWORD` (blank in the template): the first admin, created on the
  first start. While blank, no admin exists and there is no sign-up page.
- `SMTP_*`: your mail server.

Optional settings (SAML, LDAP, a trusted proxy header, AI keys, rate
limits, query timeouts) are in [ENV_VARS.md](../ENV_VARS.md).

## 2. Start

```bash
docker compose -f docker-compose.prod.yml up -d --build
docker compose -f docker-compose.prod.yml ps
docker compose -f docker-compose.prod.yml logs -f backend worker
```

Sign in at your `BASE_URL` with `ADMIN_EMAIL` and `ADMIN_PASSWORD`, then invite the rest of
your team from the admin pages.

## 3. Update

```bash
git pull
docker compose -f docker-compose.prod.yml up -d --build
```

The `migrate` service runs on every `up` and applies only pending migrations.

## 4. Back up

Postgres holds everything that matters. Back it up regularly, together with a copy of `.env`:

```bash
docker compose -f docker-compose.prod.yml exec -T postgres \
  sh -c 'pg_dump -U "$POSTGRES_USER" "$POSTGRES_DB"' > backup.sql
```

Uploaded CSV and SQLite files live in the `objectdata` volume; back it up too if you use upload
data sources.

## TLS and reverse proxy

The stack serves plain HTTP on `FRONTEND_PORT` (web app) and `BACKEND_PORT` (API). Put a reverse
proxy such as Caddy, nginx or Traefik in front of both to terminate TLS, then set `BASE_URL` and
`API_BASE_URL` to the HTTPS addresses and `TRUSTED_PROXY_HOPS=1` so rate limits see real client
addresses. Serve both from the same hostname (ports or paths may differ): session cookies are
host-only.

When the proxy runs on the same host, publish the ports on loopback only
(`FRONTEND_PORT=127.0.0.1:80`, `BACKEND_PORT=127.0.0.1:8080`) or firewall them. Otherwise clients
can skip the proxy, and with `TRUSTED_PROXY_HOPS=1` they can forge the forwarded address that rate
limits key on.

## Scaling

Workers scale horizontally: `docker compose -f docker-compose.prod.yml up -d --scale worker=3`.
For more than one backend replica, set `RATE_LIMIT_REPLICAS`. See
[ARCHITECTURE.md](../ARCHITECTURE.md#2-components) for the details.
