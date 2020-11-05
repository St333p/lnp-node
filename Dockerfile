FROM rust:1.47.0-slim-buster as builder

RUN apt-get update -y \
    && apt-get install -y \
        libsqlite3-dev \
        libssl-dev \
        libzmq3-dev \
        pkg-config \
        cmake \
        g++

ENV SRC=/usr/local/src/lnpnode

COPY doc ${SRC}/doc
COPY shell ${SRC}/shell
COPY src ${SRC}/src
COPY build.rs Cargo.lock Cargo.toml codecov.yml config_spec.toml LICENSE license_header.txt README.md ${SRC}/

WORKDIR ${SRC}

RUN cargo install --path . --bins --all-features


FROM debian:buster-slim

RUN apt-get update -y \
    && apt-get install -y \
        libzmq3-dev \
        libsqlite3-dev \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

COPY --from=builder /usr/local/cargo/bin/channeld /usr/local/bin/
COPY --from=builder /usr/local/cargo/bin/gossipd /usr/local/bin/
COPY --from=builder /usr/local/cargo/bin/lnp-cli /usr/local/bin/
COPY --from=builder /usr/local/cargo/bin/lnpd /usr/local/bin/
COPY --from=builder /usr/local/cargo/bin/peerd /usr/local/bin/
COPY --from=builder /usr/local/cargo/bin/routed /usr/local/bin/

ENV APP_DIR=/srv/app USER=lnpnode
ENV CONF=${APP_DIR}/config.toml

RUN adduser --home ${APP_DIR} --shell /bin/bash --disabled-login \
        --gecos "${USER} user" ${USER}

USER ${USER}

WORKDIR ${APP_DIR}

RUN mkdir ${APP_DIR}/.lnp_node

EXPOSE 9666 9735

ENTRYPOINT ["lnpd", "-vvvv"]
