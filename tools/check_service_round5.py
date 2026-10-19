#!/usr/bin/env python3
"""Run the existing HTTP/delta oracle with a shared mixed-case original prefix.

Arguments and isolation rules are identical to check_service_round3.py. Both
versions receive the same probe paths. Real data/full results remain local.
The mixed-case prefix forces the round-5 cold-path representation to participate
in ordinary updates, rename/delete/replace, explicit reload and recovery.
"""
import check_service_round3 as base


if __name__ == '__main__':
    base.ROOT = '/__MacEvery_Round5_Shared_Service_Probe__'
    raise SystemExit(base.main())
