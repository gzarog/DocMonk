# Nightly maintenance for the session store.
require "redis"

def purge_stale_sessions(redis, now)
  cutoff = now - 30 * 24 * 3600
  redis.scan_each(match: "sess:*") do |key|
    seen = redis.hget(key, "last_seen").to_i
    redis.del(key) if seen < cutoff
  end
end
