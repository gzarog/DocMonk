# Operations guide

## Image caching

The gallery calls `GET /sb/v1/content/groups/image` and caches the response
for ten minutes.

## Large images

Large renditions are served by /sb/v1/content/groups/image2 only.
