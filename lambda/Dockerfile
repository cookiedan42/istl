# Build context is the repo root: `docker build -t istl-lambda -f lambda/Dockerfile .`
# Build-context exclusions live in `.dockerignore` at the repo root (next to Cargo.toml).
# Default target architecture is arm64 (Graviton). For x86_64 swap the runtime
# tag to `public.ecr.aws/lambda/python:3.12-x86_64` and the builder to a
# matching x86_64 base.

# ---------- stage 1: build the istl wheel for manylinux2014 -----------------
FROM --platform=linux/arm64 ghcr.io/rust-cross/manylinux2014-cross:aarch64 AS builder

ENV PATH="/opt/python/cp312-cp312/bin:${PATH}" \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH="/usr/local/cargo/bin:${PATH}"

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain stable --profile minimal \
 && rustup target add aarch64-unknown-linux-gnu \
 && pip install --no-cache-dir 'maturin>=1.7,<2'

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN maturin build \
      --release \
      --manifest-path crates/istl-py/Cargo.toml \
      --compatibility manylinux2014 \
      --interpreter python3.12

# ---------- stage 2: AWS Lambda Python runtime ------------------------------
FROM public.ecr.aws/lambda/python:3.12-arm64

COPY --from=builder /build/target/wheels/istl-*.whl /tmp/
RUN pip install --no-cache-dir /tmp/istl-*.whl boto3 \
 && rm /tmp/istl-*.whl

COPY lambda/app.py ${LAMBDA_TASK_ROOT}/

CMD ["app.handler"]
