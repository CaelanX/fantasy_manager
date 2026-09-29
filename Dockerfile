# Container alternative to deploy/install.sh (the Ubuntu + systemd path in docs/hosting.md is the
# primary one). Used by docker-compose.yml for both the dashboard and the daily-job sidecar.
# Secrets are never baked in: .dockerignore excludes .env and data/, compose passes .env at runtime.
FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_NO_CACHE_DIR=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    FM_DATA_DIR=/app/data

RUN apt-get update \
 && apt-get install -y --no-install-recommends tzdata sqlite3 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --user-group --home-dir /app --shell /usr/sbin/nologin fantasy

WORKDIR /app
COPY pyproject.toml ./
COPY fantasy_manager ./fantasy_manager
RUN pip install ".[web]"
COPY deploy ./deploy
RUN chmod +x deploy/*.sh && install -d -m 700 -o fantasy -g fantasy /app/data /app/backups

USER fantasy
EXPOSE 8765
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
  CMD python -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8765/healthz', timeout=4)"

# Only the caddy container can reach this port (it is not published). uvicorn takes the client IP
# and https scheme from Caddy's X-Forwarded-* headers; the app then sets the Secure cookie.
CMD ["uvicorn", "fantasy_manager.web.app:app", "--host", "0.0.0.0", "--port", "8765", \
     "--proxy-headers", "--forwarded-allow-ips", "*", "--no-server-header"]
