# `istl` on AWS Lambda (container image)

A minimal container-image Lambda that reads an `.istl` file directly from S3
via range GETs, runs a bbox query, and writes the resulting `.stl` back to S3.
All the heavy lifting is in the Rust `istl` extension; the handler is ~20
lines of Python.

The container is built for **arm64 / Graviton** by default. See "Other
architectures" below to switch to x86_64.

## Files

| Path | Purpose |
|---|---|
| `Dockerfile` | Two-stage build: manylinux2014 wheel builder → `public.ecr.aws/lambda/python:3.12-arm64`. |
| `app.py` | Lambda handler. Calls `istl.query_url(src, /tmp/result.stl, bbox)` then uploads via `boto3`. |
| `event.example.json` | Sample invocation payload. |
| `../.dockerignore` | (repo root) Keeps `data/`, `target/`, the venv etc. out of the build context. |

The handler API:

```python
{
  "src_url":    "s3://bucket/key.istl",         # source .istl
  "dst_bucket": "bucket",                       # destination bucket
  "dst_key":    "path/result.stl",              # destination object key
  "bbox":       [xmin, ymin, zmin, xmax, ymax, zmax]
}
```

Use a large finite number (e.g. `1e30`) to "disable" an axis — JSON has no
`Infinity` literal. The Rust side treats `±inf` and very large finite values
the same for bbox-intersection.

## 1. Build

From the **repo root** (not from `lambda/`):

```bash
docker build -t istl-lambda -f lambda/Dockerfile .
```

First build takes a few minutes (compiles the Rust workspace in release
mode). Subsequent builds reuse the cargo cache layer.

Sanity-check the image:

```bash
docker run --rm --entrypoint python istl-lambda \
  -c "import istl; print(istl.DEFAULT_GROUP_SIZE)"
# 32768
```

## 2. Local smoke test

The AWS-provided base image ships the Lambda Runtime Interface Emulator at
`/usr/local/bin/aws-lambda-rie`, so you can invoke the function locally with
real S3 credentials:

```bash
docker run --rm -p 9000:8080 \
  -e AWS_REGION=ap-southeast-1 \
  -e AWS_ACCESS_KEY_ID=... \
  -e AWS_SECRET_ACCESS_KEY=... \
  -e AWS_SESSION_TOKEN=... \
  istl-lambda

# in another terminal:
curl -s -XPOST -d @lambda/event.example.json \
  http://localhost:9000/2015-03-31/functions/function/invocations
```

You should get back something like:

```json
{
  "src": "s3://my-bucket/terrain.istl",
  "dst": "s3://my-bucket/aoi/result.stl",
  "groups_matched": 3,
  "triangles_written": 24576
}
```

## 3. Deploy to AWS Lambda

Push to ECR and create a container-image Lambda. Replace `ACCOUNT` and
`REGION`:

```bash
# one-time: create the ECR repo
aws ecr create-repository --repository-name istl-lambda --region REGION

# every deploy: login, tag, push
aws ecr get-login-password --region REGION \
  | docker login --username AWS --password-stdin ACCOUNT.dkr.ecr.REGION.amazonaws.com

docker tag istl-lambda:latest \
  ACCOUNT.dkr.ecr.REGION.amazonaws.com/istl-lambda:latest
docker push \
  ACCOUNT.dkr.ecr.REGION.amazonaws.com/istl-lambda:latest

# create the function (first time only)
aws lambda create-function \
  --function-name istl-query \
  --package-type Image \
  --code ImageUri=ACCOUNT.dkr.ecr.REGION.amazonaws.com/istl-lambda:latest \
  --role arn:aws:iam::ACCOUNT:role/istl-lambda-role \
  --architectures arm64 \
  --memory-size 1024 \
  --timeout 60 \
  --region REGION

# update on subsequent deploys
aws lambda update-function-code \
  --function-name istl-query \
  --image-uri ACCOUNT.dkr.ecr.REGION.amazonaws.com/istl-lambda:latest \
  --region REGION
```

### Minimum IAM policy for the execution role

Attach `AWSLambdaBasicExecutionRole` for CloudWatch Logs, plus an inline
policy for S3:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": "s3:GetObject",
      "Resource": "arn:aws:s3:::SRC-BUCKET/*"
    },
    {
      "Effect": "Allow",
      "Action": "s3:PutObject",
      "Resource": "arn:aws:s3:::DST-BUCKET/*"
    }
  ]
}
```

Lambda injects `AWS_REGION` and the execution-role credentials into the
environment automatically — the `object_store` AWS backend used by
`istl.query_url` picks them up with no extra config.

### Invoke

```bash
aws lambda invoke \
  --function-name istl-query \
  --payload fileb://lambda/event.example.json \
  /tmp/out.json
cat /tmp/out.json
```

## Other architectures

To build an x86_64 image, edit `Dockerfile`:

- Builder: swap `ghcr.io/rust-cross/manylinux2014-cross:aarch64` for an
  x86_64-native builder (e.g. `quay.io/pypa/manylinux2014_x86_64`) and drop
  the `aarch64-unknown-linux-gnu` rustup target.
- Runtime: change `public.ecr.aws/lambda/python:3.12-arm64` →
  `public.ecr.aws/lambda/python:3.12-x86_64`.
- `--platform=linux/arm64` → `linux/amd64`.
- When creating the Lambda: `--architectures x86_64`.

## See also

- [`crates/istl-py/README.md`](../crates/istl-py/README.md) — Python API
  reference for `istl.query_url` and friends.
- [`CLAUDE.md`](../CLAUDE.md) — architecture notes, format spec, performance
  numbers.
