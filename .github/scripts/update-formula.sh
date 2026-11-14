#!/usr/bin/env bash
# Rewrites Formula/overseer.rb of Ephasme/homebrew-tap for the release <tag>: version, then the url and sha256 of each
# platform, from the release's SHA256SUMS. Fails if an asset or a url/sha256 pair is missing, so the formula never
# points at a file that does not exist.
#
#   update-formula.sh <tag> <path to Formula/overseer.rb>
set -euo pipefail

tag="${1:?usage: update-formula.sh <tag> <formula>}"
formula="${2:?usage: update-formula.sh <tag> <formula>}"
# The tag is interpolated into Ruby source below: a git ref may legally carry quotes or semicolons, a Ruby
# literal may not.
if [[ ! "$tag" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "FATAL: invalid tag '$tag'" >&2
  exit 1
fi
base="https://github.com/Ephasme/overseer/releases/download/$tag"
sums="$(curl -fsSL "$base/SHA256SUMS")"

ruby - "$formula" "${tag#v}" "$base" "$sums" <<'RUBY'
formula, version, base, sums = ARGV
version_pattern = /^  version "[^"]*"$/
text = File.read(formula)
# `sub` returns the text unchanged on no match, which would ship the new binaries under the old version.
abort "FATAL: no version line in #{formula}" unless text.match?(version_pattern)
text = text.sub(version_pattern, %(  version "#{version}"))
%w[aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-musl].each do |target|
  name = "overseer-#{target}.tar.gz"
  sum = sums.lines.map(&:split).find { |_, n| n == name }&.first
  abort "FATAL: #{name} is missing from SHA256SUMS of #{base}" unless sum
  # The checksum is written into Ruby source: anything but 64 lowercase hex breaks or alters the formula.
  abort "FATAL: malformed sha256 for #{name} in SHA256SUMS of #{base}: #{sum.inspect}" unless sum.match?(/\A[0-9a-f]{64}\z/)
  pattern = /^([ \t]*)url "[^"]*\/#{Regexp.escape(name)}"\n\1sha256 "[0-9a-f]{64}"$/
  abort "FATAL: no url/sha256 pair for #{name} in #{formula}" unless text.match?(pattern)
  text = text.sub(pattern) { %(#{$1}url "#{base}/#{name}"\n#{$1}sha256 "#{sum}") }
end
# Rename over the formula: a run interrupted mid-write must not leave the tap with a truncated file to commit.
tmp = "#{formula}.tmp"
File.write(tmp, text)
File.rename(tmp, formula)
RUBY

grep -nE '^\s*(version|url|sha256) ' "$formula"
