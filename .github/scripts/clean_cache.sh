#!/usr/bin/env bash

set -eo pipefail

# Script to clean up old cache directories in `$RUNNER_TOOL_CACHE/$GITHUB_REPOSITORY`, for our selfhosted this is "/home/docker/actions-runner/_work/_tool/Irys-xyz/irys"
# Removes directories prefixed with "ci-cache-", containing cache.tgz files based on last access time
# The clearing threshold decreases as the total directory size increases - default is 7 days, but the time decreases until a minimum of 4 days at 30GB of cache.

if [ -n "$1" ]; then
    dir="$1"
else
    # Check if both env vars are set
    if [ -z "$GITHUB_REPOSITORY" ] || [ -z "$RUNNER_TOOL_CACHE" ]; then
        echo "Error: GITHUB_REPOSITORY and RUNNER_TOOL_CACHE must be set if no directory argument is provided"
        exit 1
    fi
    dir="$RUNNER_TOOL_CACHE/$GITHUB_REPOSITORY"
fi

# Set prefix
prefix="${2:-ci-cache-}"

echo "using directory: $dir, prefix $prefix"


# Check if provided path exists and is a directory
if [ ! -d "$dir" ]; then
    echo "Error: '$dir' is not a directory or doesn't exist"
    exit 0
fi

# Get total size of directory in GB
total_size_kb=$(du -s "$dir" | cut -f1)
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
    # Linear interpolation
    reduction_factor=$(echo "scale=4; $total_size_gb/$size_threshold" | bc)
    seconds_range=$((base_seconds - min_seconds))
    reduction=$(echo "scale=0; $seconds_range * $reduction_factor" | bc)
    cutoff_seconds=$((base_seconds - reduction))
fi

# Get current time in seconds since epoch
current_time=$(date +%s)

# Counter for deleted directories
deleted_count=0
skipped_count=0

echo "Directory size: ${total_size_gb}GB"
echo "Using cutoff time of $(echo "scale=1; $cutoff_seconds/86400" | bc) days"
echo "Looking for directories with prefix: $prefix"

# Process each subdirectory
for dir in "$dir"/*/; do
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

    cache_file="$dir/cache.tgz"
    
    # Check if cache.tgz exists
    if [ ! -f "$cache_file" ]; then
        echo "Warning: $dir does not contain cache.tgz, skipping..."
        continue
    fi

    # Get last access time of cache.tgz
    last_access=$(stat -c %X "$cache_file")
    
    # Calculate time difference
    time_diff=$((current_time - last_access))
    
    # If file hasn't been accessed within cutoff time, remove the directory
    if [ $time_diff -gt $cutoff_seconds ]; then
        echo "Removing $dir (last accessed $(date -d "@$last_access"))"
        rm -rf "$dir"
        deleted_count=$((deleted_count + 1))
    fi
done

echo "Cleanup complete. Removed $deleted_count directories."
echo "Skipped $skipped_count directories not matching prefix '$prefix'"