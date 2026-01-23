FROM rust:1.97.0-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git \
        gh \
        curl \
        ca-certificates \
        gnupg \
        pkg-config \
        libssl-dev \
    && mkdir -p /etc/apt/keyrings \
    && curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key \
        | gpg --dearmor -o /etc/apt/keyrings/nodesource.gpg \
    && echo "deb [signed-by=/etc/apt/keyrings/nodesource.gpg] https://deb.nodesource.com/node_22.x nodistro main" \
        > /etc/apt/sources.list.d/nodesource.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        nodejs \
    && npm install -g @opencode-ai/cli \
    && rm -rf /var/lib/apt/lists/*

ARG USERNAME=dev
ARG USER_UID=1000
ARG USER_GID=1000

RUN groupadd --gid ${USER_GID} ${USERNAME} \
    && useradd --uid ${USER_UID} --gid ${USER_GID} -m ${USERNAME}

# Give the non-root user write access to the registry cache mount point.
# A fresh named volume mounted here inherits this ownership, so `cargo`
# can populate the cache without permission errors.
ENV CARGO_HOME=/home/dev/.cargo
RUN mkdir -p /home/dev/.cargo/registry /home/dev/.cargo/git \
    && chown -R ${USER_UID}:${USER_GID} /home/dev/.cargo

WORKDIR /workspace
USER ${USERNAME}

CMD ["bash"]