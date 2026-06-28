import os

import boto3
import istl

s3 = boto3.client("s3")


def handler(event, _context):
    src_url = event["src_url"]
    dst_bucket = event["dst_bucket"]
    dst_key = event["dst_key"]
    bbox = tuple(event["bbox"])

    out_path = "/tmp/result.stl"
    stats = istl.query_url(src_url, out_path, bbox)

    s3.upload_file(out_path, dst_bucket, dst_key)
    os.remove(out_path)

    return {
        "src": src_url,
        "dst": f"s3://{dst_bucket}/{dst_key}",
        **stats,
    }
