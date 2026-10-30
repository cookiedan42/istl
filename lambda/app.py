import istl


def handler(event, _context):
    src_url = event["src_url"]
    dst_url = f"s3://{event['dst_bucket']}/{event['dst_key']}"
    bbox = tuple(event["bbox"])
    fmt = event.get("format", "stl")

    stats = istl.query(src_url, dst_url, bbox, fmt)

    return {
        "src": src_url,
        "dst": dst_url,
        **stats,
    }
