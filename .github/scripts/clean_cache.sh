#!/usr/bin/env bash

set -eo pipefail

# Script to clean up old cache directories in `$RUNNER_TOOL_CACHE/$GITHUB_REPOSITORY`, for our selfhosted this is "/home/docker/actions-runner/_work/_tool/Irys-xyz/irys"
# Removes directories prefixed with "ci-cache-", containing cache.tgz files based on last access time
# The clearing threshold decreases as the total directory size increases - default is 7 days, but the time decreases until a minimum of 4 days at 30GB of cache.

if [ -n "$1" ]; then
    cache_root_dir="$1"
else
    # Check if both env vars are set
    if [ -z "$GITHUB_REPOSITORY" ] || [ -z "$RUNNER_TOOL_CACHE" ]; then
        echo "Error: GITHUB_REPOSITORY and RUNNER_TOOL_CACHE must be set if no directory argument is provided"
        exit 1
    fi
    cache_root_dir="$RUNNER_TOOL_CACHE/$GITHUB_REPOSITORY"
fi

# Set prefix
prefix="${2:-ci-cache-}"

echo "using directory: $cache_root_dir, prefix $prefix"


# Check if provided path exists and is a directory
if [ ! -d "$cache_root_dir" ]; then
    echo "Error: '$cache_root_dir' is not a directory or doesn't exist"
    exit 0
fi

# Get total size of directory in GB
total_size_kb=$(du -s "$cache_root_dir" | cut -f1)
total_size_gb=$(echo "scale=2; $total_size_kb/1024/1024" | bc)

# Calculate cutoff time based on directory size
# Base: 7 days (604800 seconds)
# At 30GB: 4 days (345600 seconds)
# Linear interpolation between these points
base_seconds=$((7 * 24 * 60 * 60))     # 7 days in seconds
min_seconds=$((4 * 24 * 60 * 60))      # 4 days in seconds
size_threshold=30                       # Size in GB where we want minimum time

# Calculate actual cutoff time based on current size
if (( $(echo "$total_size_gb >= $size_threshold" | bc -l) )); then
    cutoff_seconds=$min_seconds
else
    # Linear interpolation using bc for floating point arithmetic
    reduction=$(echo "scale=0; ($total_size_gb/$size_threshold) * ($base_seconds - $min_seconds)" | bc)
    cutoff_seconds=$(echo "$base_seconds - $reduction" | bc)
fi

deleted_count=0
skipped_count=0

echo "Directory size: ${total_size_gb}GB"
echo "Using cutoff time of $(echo "scale=1; $cutoff_seconds/86400" | bc) days"
echo "Looking for directories with prefix: $prefix"

# Process each subdirectory
for dir in "$cache_root_dir"/*/; do
    if [ ! -d "$dir" ]; then
        continue
    fi

    # Get just the directory name without path
    dirname=$(basename "$dir")
    
    # Skip if directory doesn't start with prefix
    if [[ ! "$dirname" == "$prefix"* ]]; then
        skipped_count=$((skipped_count + 1))
        continue
    fi

    # Find the most recently modified cache file in the directory
    latest_mtime=$(find "$dir" -type f -name "cache*" -printf '%T@\n' 2>/dev/null | sort -nr | head -1)
    
    # If no cache files found, skip this directory
    if [ -z "$latest_mtime" ]; then
        echo "Warning: $dir does not contain any cache files, skipping..."
        continue
    fi

    # Remove decimal part from timestamp for integer comparison
    latest_mtime=${latest_mtime%.*}
    
    # Calculate time difference
    time_diff=$((current_time - latest_mtime))
    
    # If no cache files have been modified within cutoff time, remove the directory
    if [ $time_diff -gt $cutoff_seconds ]; then
        echo "Removing $dir (last modified $(date -d "@$latest_mtime"))"
        rm -rf "$dir"
        deleted_count=$((deleted_count + 1))
    fi
done

echo "Cleanup complete. Removed $deleted_count directories."
echo "Skipped $skipped_count directories not matching prefix '$prefix'"