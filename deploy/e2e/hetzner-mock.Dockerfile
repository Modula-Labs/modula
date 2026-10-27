# The fake cloud for deploy/e2e/migrate.sh: hetzner-mock.py with the Docker
# CLI (servers are containers, created over the host's Docker socket).
FROM alpine:3.22
RUN apk add --no-cache python3 docker-cli curl openssl iproute2 jq
COPY hetzner-mock.py /mock.py
CMD ["python3", "-u", "/mock.py"]
