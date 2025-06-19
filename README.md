# bcachefs-exporter

A Prometheus exporter for [bcachefs](https://bcachefs.org/).

Root is not needed, we only read from `/sys/fs/bcachefs`, which is world-readable.

Tested with Linux kernel 6.15.1.

## Collected data
- for each device/disk:
  - usage categories (useful to look at rebalance behavior)
  - capacity

The Grafana dashboard also shows some disk IO stats with each device, collected by [node_exporter](https://github.com/prometheus/node_exporter).
