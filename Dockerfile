# Lightweight stdio launcher for MCP directory build checks (Glama and others).
# Installs the published dreamd-mcp npm shim, which downloads and sha256-verifies
# the prebuilt dreamd binary at build time, so the container needs no network
# at run time. The server starts in an empty /workspace with no `setup` step
# and answers initialize and tools/list over stdio. Bump the version with each
# npm release.
#
# From 1.2.1 the prebuilt linux-x86_64 binary is static, so any Node base
# works, Alpine included. 1.2.0 needed glibc 2.39, which is why this base is
# Debian 13 (trixie).
FROM node:22-trixie-slim

ARG DREAMD_MCP_VERSION=1.2.1

RUN npm install -g "dreamd-mcp@${DREAMD_MCP_VERSION}" \
    && dreamd-mcp version

WORKDIR /workspace

ENTRYPOINT ["dreamd-mcp"]
