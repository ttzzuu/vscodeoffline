# --------------------------------------------------------------------------------
# STAGE 1: Builder
# --------------------------------------------------------------------------------
FROM postgres:17-bookworm AS builder

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# --------------------------------------------------------------------------------
# VERSION PINS
# --------------------------------------------------------------------------------
ARG PG_JOBMON_VERSION=1.4.1
ARG PG_PARTMAN_VERSION=5.2.2
ARG TIMESCALEDB_VERSION=2.17.2
ARG POSTGIS_VERSION=3.5.0
ARG PG_CRON_VERSION=1.6.4

# --------------------------------------------------------------------------------
# 1. Install Build Dependencies
# --------------------------------------------------------------------------------
# Note: switched to postgresql-server-dev-17
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    wget \
    clang \
    llvm \
    libkrb5-dev \
    libssl-dev \
    libxml2-dev \
    cmake \
    # PostGIS Deps
    libgeos-dev \
    libproj-dev \
    libgdal-dev \
    libprotobuf-c-dev \
    protobuf-c-compiler \
    libjson-c-dev \
    postgresql-server-dev-17 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
RUN mkdir -p /build_artifacts

# --------------------------------------------------------------------------------
# Extension 1: pg_jobmon
# --------------------------------------------------------------------------------
RUN wget -O pg_jobmon.tar.gz "https://github.com/omniti-labs/pg_jobmon/archive/refs/tags/v${PG_JOBMON_VERSION}.tar.gz" && \
    mkdir pg_jobmon && \
    tar -xzf pg_jobmon.tar.gz -C pg_jobmon --strip-components=1 && \
    cd pg_jobmon && \
    make && \
    make install DESTDIR=/build_artifacts

# --------------------------------------------------------------------------------
# Extension 2: pg_partman
# --------------------------------------------------------------------------------
RUN wget -O pg_partman.tar.gz "https://github.com/pgpartman/pg_partman/archive/refs/tags/v${PG_PARTMAN_VERSION}.tar.gz" && \
    mkdir pg_partman && \
    tar -xzf pg_partman.tar.gz -C pg_partman --strip-components=1 && \
    cd pg_partman && \
    make && \
    make install DESTDIR=/build_artifacts

# --------------------------------------------------------------------------------
# Extension 3: pg_cron
# --------------------------------------------------------------------------------
RUN wget -O pg_cron.tar.gz "https://github.com/citusdata/pg_cron/archive/refs/tags/v${PG_CRON_VERSION}.tar.gz" && \
    mkdir pg_cron && \
    tar -xzf pg_cron.tar.gz -C pg_cron --strip-components=1 && \
    cd pg_cron && \
    make && \
    make install DESTDIR=/build_artifacts

# --------------------------------------------------------------------------------
# Extension 4: TimescaleDB
# --------------------------------------------------------------------------------
# TimescaleDB 2.17.x supports PG17
RUN wget -O timescaledb.tar.gz "https://github.com/timescale/timescaledb/archive/refs/tags/${TIMESCALEDB_VERSION}.tar.gz" && \
    mkdir timescaledb && \
    tar -xzf timescaledb.tar.gz -C timescaledb --strip-components=1 && \
    cd timescaledb && \
    # Bootstrap handles CMake generation
    ./bootstrap -DREGRESS_CHECKS=OFF -DTAP_CHECKS=OFF -DWARNINGS_AS_ERRORS=OFF && \
    cd build && \
    make && \
    make install DESTDIR=/build_artifacts

# --------------------------------------------------------------------------------
# Extension 5: PostGIS
# --------------------------------------------------------------------------------
RUN wget -O postgis.tar.gz "https://download.osgeo.org/postgis/source/postgis-${POSTGIS_VERSION}.tar.gz" && \
    mkdir postgis && \
    tar -xzf postgis.tar.gz -C postgis --strip-components=1 && \
    cd postgis && \
    ./configure \
        --without-gui \
        --without-raster \
        --with-protobuf \
    && \
    make -j$(nproc) && \
    make install DESTDIR=/build_artifacts


# --------------------------------------------------------------------------------
# STAGE 2: Final Image
# --------------------------------------------------------------------------------
FROM postgres:17-bookworm

# 1. Install Runtime Dependencies
# These are the shared libraries (.so) required by the compiled extensions.
# Since we are still on 'bookworm', these package names generally remain the same.
RUN apt-get update && apt-get install -y --no-install-recommends \
    libgeos-c1v5 \
    libproj25 \
    libgdal32 \
    libprotobuf-c1 \
    libjson-c5 \
    libxml2 \
    libssl3 \
    libpq5 \
    && rm -rf /var/lib/apt/lists/*

# 2. Copy artifacts
# This recursively copies usr/lib, usr/share, etc., merging them into the filesystem.
COPY --from=builder /build_artifacts /

# 3. Setup Config
# TimescaleDB and pg_cron MUST be loaded on startup via shared_preload_libraries.
RUN echo "shared_preload_libraries = 'timescaledb,pg_cron,pg_partman_bgw'" >> /usr/share/postgresql/postgresql.conf.sample

# 4. Cron Database Setup
# pg_cron expects a database named 'postgres' by default to store its metadata.
CMD ["postgres"]
