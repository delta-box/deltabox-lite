## Experiment 2 — scalability

Latency cells are p50 / p95 ms; byte cells are medians. "largest / smallest" compares p50 (or medians) at the two ends of the sweep.

### Case 2A (file size, 4 KiB edit) — commit

**Total (write + commit) ms**

| system | 64 KiB | 1 MiB | 16 MiB | 64 MiB | 256 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| bare | 2.2 / 3.0 | 2.0 / 2.5 | 2.3 / 2.8 | 2.1 / 2.6 | 2.0 / 2.6 | 0.9× |
| bare +fsync | 4.2 / 4.5 | 3.7 / 4.6 | 3.8 / 4.8 | 4.4 / 4.6 | 4.3 / 5.3 | 1.0× |
| reflink (XFS) | 3.8 / 4.7 | 2.8 / 3.7 | 3.2 / 4.7 | 4.3 / 5.2 | 2.8 / 3.7 | 0.7× |
| reflink (XFS) +fsync | 10.7 / 13.1 | 10.6 / 11.8 | 10.6 / 12.3 | 10.0 / 10.9 | 9.2 / 10.3 | 0.9× |
| raw Sandlock | 6.4 / 8.9 | 7.4 / 9.9 | 22.6 / 24.6 | 66.2 / 78.4 | 735.4 / 1340.9 | 115.3× |
| cbcow-through | 8.1 / 10.7 | 7.7 / 8.8 | 7.5 / 9.5 | 7.9 / 9.6 | 8.3 / 9.7 | 1.0× |
| cbcow-through durable | 149.7 / 185.5 | 142.5 / 147.9 | 158.8 / 167.0 | 153.6 / 165.8 | 169.5 / 233.8 | 1.1× |
| cbcow-private | 11.3 / 12.2 | 52.6 / 55.7 | 51.1 / 55.5 | 149.8 / 153.3 | 1523.8 / 2011.4 | 134.7× |
| cbcow-private durable | 356.8 / 458.6 | 382.7 / 491.9 | 437.2 / 604.8 | 719.7 / 1624.9 | 2116.3 / 2952.7 | 5.9× |

**Write phase ms**

| system | 64 KiB | 1 MiB | 16 MiB | 64 MiB | 256 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| bare | 2.2 / 3.0 | 2.0 / 2.5 | 2.3 / 2.8 | 2.1 / 2.6 | 2.0 / 2.6 | 0.9× |
| bare +fsync | 4.2 / 4.5 | 3.7 / 4.6 | 3.8 / 4.8 | 4.4 / 4.6 | 4.3 / 5.3 | 1.0× |
| reflink (XFS) | 3.7 / 4.7 | 2.8 / 3.6 | 3.1 / 4.6 | 4.2 / 5.1 | 2.8 / 3.7 | 0.7× |
| reflink (XFS) +fsync | 8.1 / 10.8 | 8.3 / 9.4 | 8.1 / 9.8 | 7.7 / 8.6 | 6.9 / 7.7 | 0.8× |
| raw Sandlock | 5.1 / 6.7 | 5.4 / 7.7 | 10.2 / 11.3 | 21.9 / 27.1 | 69.9 / 74.6 | 13.8× |
| cbcow-through | 6.7 / 8.7 | 6.3 / 7.3 | 6.1 / 8.0 | 6.4 / 8.0 | 7.0 / 8.2 | 1.0× |
| cbcow-through durable | 35.2 / 37.1 | 33.8 / 36.7 | 37.2 / 41.8 | 35.9 / 41.9 | 35.8 / 53.1 | 1.0× |
| cbcow-private | 6.3 / 7.1 | 7.7 / 10.8 | 8.4 / 11.3 | 12.0 / 13.8 | 12.8 / 14.7 | 2.0× |
| cbcow-private durable | 36.3 / 51.1 | 34.0 / 49.2 | 34.0 / 37.2 | 32.2 / 39.4 | 31.3 / 40.8 | 0.9× |

**Commit phase ms**

| system | 64 KiB | 1 MiB | 16 MiB | 64 MiB | 256 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| bare | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | – |
| bare +fsync | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | – |
| reflink (XFS) | 0.1 / 0.1 | 0.0 / 0.0 | 0.0 / 0.1 | 0.1 / 0.1 | 0.0 / 0.1 | 0.4× |
| reflink (XFS) +fsync | 2.4 / 2.7 | 2.4 / 2.6 | 2.3 / 2.6 | 2.3 / 2.5 | 2.3 / 2.7 | 1.0× |
| raw Sandlock | 1.4 / 2.1 | 1.9 / 2.6 | 12.1 / 13.9 | 44.0 / 51.4 | 667.7 / 1274.0 | 488.8× |
| cbcow-through | 1.4 / 2.1 | 1.3 / 1.9 | 1.3 / 1.9 | 1.5 / 1.8 | 1.4 / 1.5 | 1.0× |
| cbcow-through durable | 115.1 / 153.9 | 108.5 / 113.2 | 121.9 / 127.6 | 117.3 / 127.3 | 129.3 / 146.8 | 1.1× |
| cbcow-private | 5.0 / 5.3 | 44.8 / 45.3 | 42.9 / 44.4 | 137.6 / 140.4 | 1509.5 / 1999.2 | 304.1× |
| cbcow-private durable | 318.9 / 426.1 | 348.3 / 442.9 | 404.2 / 569.8 | 688.4 / 1588.8 | 2083.0 / 2918.9 | 6.5× |

**Disk write I/O**

| system | 64 KiB | 1 MiB | 16 MiB | 64 MiB | 256 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| bare | 64 KiB | 64 KiB | 64 KiB | 64 KiB | 64 KiB | 1.0× |
| bare +fsync | 64 KiB | 64 KiB | 64 KiB | 64 KiB | 64 KiB | 1.0× |
| reflink (XFS) | 4 KiB | 4 KiB | 4 KiB | 4 KiB | 4 KiB | 1.0× |
| reflink (XFS) +fsync | 4 KiB | 4 KiB | 4 KiB | 4 KiB | 4 KiB | 1.0× |
| raw Sandlock | 132 KiB | 2.0 MiB | 32.0 MiB | 128.0 MiB | 512.0 MiB | 3971.9× |
| cbcow-through | 104 KiB | 104 KiB | 104 KiB | 104 KiB | 104 KiB | 1.0× |
| cbcow-through durable | 108 KiB | 108 KiB | 108 KiB | 108 KiB | 108 KiB | 1.0× |
| cbcow-private | 348 KiB | 3.2 MiB | 48.2 MiB | 192.2 MiB | 768.2 MiB | 2260.5× |
| cbcow-private durable | 352 KiB | 3.2 MiB | 48.2 MiB | 192.2 MiB | 768.2 MiB | 2234.6× |

**Peak CoW / undo storage**

| system | 64 KiB | 1 MiB | 16 MiB | 64 MiB | 256 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| bare | 0 B | 0 B | 0 B | 0 B | 0 B | – |
| bare +fsync | 0 B | 0 B | 0 B | 0 B | 0 B | – |
| reflink (XFS) | 64 KiB | 128 KiB | 128 KiB | 128 KiB | 128 KiB | 2.0× |
| reflink (XFS) +fsync | 64 KiB | 128 KiB | 128 KiB | 128 KiB | 128 KiB | 2.0× |
| raw Sandlock | 76 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 256.0 MiB | 3449.4× |
| cbcow-through | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 1.0× |
| cbcow-through durable | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 1.0× |
| cbcow-private | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 1.0× |
| cbcow-private durable | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 1.0× |

### Case 2A (file size, 4 KiB edit) — abort

**Total (write + abort) ms**

| system | 64 KiB | 1 MiB | 16 MiB | 64 MiB | 256 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| reflink (XFS) | 2.9 / 3.8 | 2.8 / 3.5 | 3.5 / 4.9 | 3.1 / 4.0 | 2.9 / 3.9 | 1.0× |
| reflink (XFS) +fsync | 7.1 / 7.9 | 7.6 / 9.8 | 6.8 / 8.2 | 6.6 / 8.4 | 6.5 / 7.8 | 0.9× |
| raw Sandlock | 5.0 / 6.2 | 5.1 / 7.3 | 10.0 / 10.8 | 22.8 / 25.8 | 71.9 / 74.0 | 14.3× |
| cbcow-through | 8.7 / 10.7 | 7.7 / 9.7 | 8.5 / 9.9 | 9.1 / 10.2 | 8.5 / 10.9 | 1.0× |
| cbcow-through durable | 203.5 / 215.0 | 227.2 / 239.5 | 216.6 / 233.8 | 249.1 / 302.5 | 204.1 / 207.9 | 1.0× |
| cbcow-private | 8.3 / 11.3 | 7.4 / 9.8 | 8.8 / 10.7 | 8.6 / 13.3 | 8.7 / 10.8 | 1.1× |
| cbcow-private durable | 204.9 / 215.1 | 242.4 / 292.3 | 229.9 / 297.2 | 200.5 / 208.0 | 212.4 / 221.3 | 1.0× |

**Write phase ms**

| system | 64 KiB | 1 MiB | 16 MiB | 64 MiB | 256 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| reflink (XFS) | 2.9 / 3.7 | 2.8 / 3.4 | 3.4 / 4.9 | 3.1 / 4.0 | 2.9 / 3.8 | 1.0× |
| reflink (XFS) +fsync | 7.1 / 7.9 | 7.5 / 9.8 | 6.8 / 8.1 | 6.6 / 8.3 | 6.5 / 7.8 | 0.9× |
| raw Sandlock | 5.0 / 6.1 | 5.1 / 7.2 | 9.2 / 10.4 | 20.5 / 22.6 | 65.2 / 67.4 | 13.0× |
| cbcow-through | 6.9 / 8.8 | 6.1 / 8.2 | 6.9 / 8.3 | 7.3 / 8.4 | 7.0 / 8.2 | 1.0× |
| cbcow-through durable | 34.5 / 39.7 | 36.7 / 41.5 | 38.3 / 42.8 | 37.9 / 100.7 | 34.9 / 36.9 | 1.0× |
| cbcow-private | 6.7 / 9.5 | 5.8 / 8.2 | 7.2 / 8.8 | 6.9 / 10.8 | 7.2 / 9.2 | 1.1× |
| cbcow-private durable | 34.8 / 37.8 | 37.7 / 49.4 | 37.6 / 45.6 | 34.5 / 38.7 | 35.9 / 38.7 | 1.0× |

**Abort phase ms**

| system | 64 KiB | 1 MiB | 16 MiB | 64 MiB | 256 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| reflink (XFS) | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.1 | 0.0 / 0.0 | 0.0 / 0.0 | 0.9× |
| reflink (XFS) +fsync | 0.0 / 0.1 | 0.0 / 0.1 | 0.0 / 0.1 | 0.0 / 0.1 | 0.0 / 0.1 | 0.8× |
| raw Sandlock | 0.0 / 0.1 | 0.1 / 0.1 | 0.5 / 1.3 | 1.9 / 3.5 | 6.6 / 7.1 | 187.1× |
| cbcow-through | 1.7 / 2.4 | 1.5 / 1.8 | 1.6 / 2.7 | 1.6 / 2.3 | 1.5 / 2.4 | 0.9× |
| cbcow-through durable | 168.0 / 174.0 | 191.5 / 203.3 | 179.6 / 195.6 | 208.9 / 236.7 | 169.2 / 174.3 | 1.0× |
| cbcow-private | 1.4 / 1.8 | 1.5 / 1.8 | 1.5 / 2.4 | 1.7 / 2.3 | 1.6 / 2.0 | 1.1× |
| cbcow-private durable | 171.2 / 179.9 | 203.6 / 255.0 | 194.0 / 254.4 | 165.6 / 171.9 | 175.9 / 183.9 | 1.0× |

**Disk write I/O**

| system | 64 KiB | 1 MiB | 16 MiB | 64 MiB | 256 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| reflink (XFS) | 4 KiB | 4 KiB | 4 KiB | 4 KiB | 4 KiB | 1.0× |
| reflink (XFS) +fsync | 4 KiB | 4 KiB | 4 KiB | 4 KiB | 4 KiB | 1.0× |
| raw Sandlock | 68 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 256.0 MiB | 3855.1× |
| cbcow-through | 120 KiB | 120 KiB | 120 KiB | 120 KiB | 120 KiB | 1.0× |
| cbcow-through durable | 124 KiB | 124 KiB | 124 KiB | 124 KiB | 124 KiB | 1.0× |
| cbcow-private | 120 KiB | 120 KiB | 120 KiB | 120 KiB | 184 KiB | 1.5× |
| cbcow-private durable | 124 KiB | 124 KiB | 124 KiB | 124 KiB | 124 KiB | 1.0× |

**Peak CoW / undo storage**

| system | 64 KiB | 1 MiB | 16 MiB | 64 MiB | 256 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| reflink (XFS) | 64 KiB | 128 KiB | 128 KiB | 128 KiB | 128 KiB | 2.0× |
| reflink (XFS) +fsync | 64 KiB | 128 KiB | 128 KiB | 128 KiB | 128 KiB | 2.0× |
| raw Sandlock | 76 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 256.0 MiB | 3449.4× |
| cbcow-through | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 1.0× |
| cbcow-through durable | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 1.0× |
| cbcow-private | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 1.0× |
| cbcow-private durable | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 8 KiB | 1.0× |

### Case 2B (write size, 256 MiB file) — commit

**Total (write + commit) ms**

| system | 4 KiB | 64 KiB | 1 MiB | 16 MiB | 64 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| bare | 2.1 / 2.7 | 1.8 / 2.2 | 2.8 / 3.6 | 7.2 / 8.0 | 22.7 / 25.3 | 10.7× |
| bare +fsync | 4.0 / 4.5 | 2.9 / 3.4 | 4.1 / 4.7 | 13.9 / 15.9 | 48.3 / 56.1 | 12.2× |
| reflink (XFS) | 3.0 / 3.9 | 2.7 / 3.0 | 3.7 / 4.4 | 9.0 / 10.5 | 27.0 / 31.0 | 9.1× |
| reflink (XFS) +fsync | 10.2 / 11.4 | 9.4 / 11.1 | 10.8 / 12.7 | 26.8 / 29.2 | 71.7 / 80.1 | 7.0× |
| raw Sandlock | 889.8 / 1282.8 | 878.7 / 1271.5 | 869.9 / 1337.4 | 892.9 / 1219.4 | 905.5 / 1314.9 | 1.0× |
| cbcow-through | 7.3 / 9.2 | 8.7 / 9.9 | 10.7 / 12.3 | 31.7 / 33.8 | 81.0 / 90.3 | 11.1× |
| cbcow-through durable | 140.1 / 146.6 | 160.2 / 185.0 | 185.0 / 234.9 | 565.8 / 807.4 | 1922.7 / 2621.3 | 13.7× |
| cbcow-private | 1385.8 / 1625.8 | 1391.6 / 1700.2 | 1293.0 / 1495.3 | 1539.0 / 1825.2 | 1327.4 / 1925.2 | 1.0× |
| cbcow-private durable | 1863.2 / 3167.7 | 2134.5 / 3124.8 | 2234.5 / 3119.8 | 2522.0 / 3761.3 | 4311.9 / 4838.3 | 2.3× |

**Write phase ms**

| system | 4 KiB | 64 KiB | 1 MiB | 16 MiB | 64 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| bare | 2.1 / 2.7 | 1.8 / 2.2 | 2.8 / 3.6 | 7.2 / 8.0 | 22.7 / 25.3 | 10.7× |
| bare +fsync | 4.0 / 4.5 | 2.9 / 3.4 | 4.1 / 4.7 | 13.9 / 15.9 | 48.3 / 56.1 | 12.2× |
| reflink (XFS) | 2.9 / 3.9 | 2.6 / 3.0 | 3.6 / 4.4 | 8.9 / 10.4 | 26.6 / 30.7 | 9.0× |
| reflink (XFS) +fsync | 7.8 / 9.0 | 7.1 / 8.6 | 8.4 / 10.2 | 24.3 / 26.6 | 68.5 / 76.1 | 8.8× |
| raw Sandlock | 70.0 / 75.9 | 69.0 / 73.1 | 69.0 / 74.7 | 74.8 / 79.0 | 86.6 / 93.9 | 1.2× |
| cbcow-through | 5.8 / 7.7 | 7.3 / 8.5 | 9.3 / 10.8 | 29.7 / 31.9 | 78.1 / 88.1 | 13.5× |
| cbcow-through durable | 31.4 / 36.9 | 35.1 / 50.9 | 39.8 / 52.8 | 425.3 / 594.6 | 1679.8 / 2184.3 | 53.5× |
| cbcow-private | 13.0 / 15.1 | 12.6 / 14.5 | 16.2 / 18.1 | 34.1 / 36.7 | 89.0 / 96.4 | 6.8× |
| cbcow-private durable | 30.0 / 33.5 | 29.3 / 38.3 | 35.3 / 97.2 | 419.9 / 591.1 | 1672.8 / 2462.8 | 55.7× |

**Commit phase ms**

| system | 4 KiB | 64 KiB | 1 MiB | 16 MiB | 64 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| bare | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | – |
| bare +fsync | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | 0.0 / 0.0 | – |
| reflink (XFS) | 0.0 / 0.1 | 0.0 / 0.0 | 0.0 / 0.0 | 0.1 / 0.2 | 0.3 / 0.5 | 9.0× |
| reflink (XFS) +fsync | 2.4 / 2.8 | 2.4 / 2.8 | 2.4 / 2.6 | 2.5 / 3.0 | 3.2 / 3.6 | 1.4× |
| raw Sandlock | 824.3 / 1211.7 | 807.5 / 1206.7 | 798.9 / 1268.1 | 819.0 / 1143.4 | 816.0 / 1226.9 | 1.0× |
| cbcow-through | 1.4 / 1.6 | 1.4 / 1.7 | 1.4 / 1.9 | 2.0 / 2.2 | 2.1 / 4.5 | 1.6× |
| cbcow-through durable | 108.2 / 113.0 | 125.7 / 145.1 | 142.4 / 197.0 | 140.5 / 219.0 | 237.4 / 487.0 | 2.2× |
| cbcow-private | 1373.4 / 1612.0 | 1381.4 / 1686.4 | 1274.8 / 1477.4 | 1507.3 / 1789.6 | 1236.6 / 1835.3 | 0.9× |
| cbcow-private durable | 1833.2 / 3138.0 | 2104.6 / 3098.1 | 2165.2 / 3086.5 | 2068.3 / 3351.7 | 2119.1 / 3157.1 | 1.2× |

**Disk write I/O**

| system | 4 KiB | 64 KiB | 1 MiB | 16 MiB | 64 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| bare | 64 KiB | 64 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 1024.0× |
| bare +fsync | 64 KiB | 64 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 1024.0× |
| reflink (XFS) | 4 KiB | 64 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 16384.0× |
| reflink (XFS) +fsync | 4 KiB | 64 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 16384.0× |
| raw Sandlock | 512.0 MiB | 512.0 MiB | 512.0 MiB | 512.0 MiB | 512.0 MiB | 1.0× |
| cbcow-through | 104 KiB | 224 KiB | 3.0 MiB | 48.0 MiB | 192.1 MiB | 1891.1× |
| cbcow-through durable | 108 KiB | 228 KiB | 3.0 MiB | 48.2 MiB | 192.6 MiB | 1825.8× |
| cbcow-private | 768.2 MiB | 768.3 MiB | 771.1 MiB | 816.1 MiB | 960.1 MiB | 1.2× |
| cbcow-private durable | 768.2 MiB | 768.3 MiB | 771.1 MiB | 816.2 MiB | 960.6 MiB | 1.3× |

**Peak CoW / undo storage**

| system | 4 KiB | 64 KiB | 1 MiB | 16 MiB | 64 MiB | largest / smallest |
|---|---:|---:|---:|---:|---:|---:|
| bare | 0 B | 0 B | 0 B | 0 B | 0 B | – |
| bare +fsync | 0 B | 0 B | 0 B | 0 B | 0 B | – |
| reflink (XFS) | 128 KiB | 128 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 512.0× |
| reflink (XFS) +fsync | 128 KiB | 128 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 512.0× |
| raw Sandlock | 256.0 MiB | 256.0 MiB | 256.0 MiB | 256.0 MiB | 256.0 MiB | 1.0× |
| cbcow-through | 8 KiB | 68 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 8197.0× |
| cbcow-through durable | 8 KiB | 68 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 8197.5× |
| cbcow-private | 8 KiB | 68 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 8197.0× |
| cbcow-private durable | 8 KiB | 68 KiB | 1.0 MiB | 16.0 MiB | 64.0 MiB | 8197.5× |

## Experiment 3 — commit-rate sweep

Commit ratio is a synthetic, controlled parameter. Each cell sums `total_round_ms` over all rounds at that ratio (seconds).

| system | 0% | 25% | 50% | 75% | 100% |
|---|---:|---:|---:|---:|---:|
| Git + bwrap | 30.86 | 30.86 | 30.63 | 30.56 | 30.56 |
| raw Sandlock (1 run, unified) | 5.76 | 6.49 | 7.18 | 7.89 | 8.87 |
| cbcow-through | 16.70 | 16.71 | 16.51 | 16.47 | 16.96 |
| cbcow-through durable | 86.70 | 81.44 | 75.19 | 71.24 | 65.49 |
| cbcow-private | 18.75 | 20.79 | 22.81 | 24.97 | 26.78 |
| cbcow-private durable | 339.36 | 355.20 | 367.39 | 376.87 | 389.57 |
| raw Sandlock (16 runs) [session-reuse ref] | – | – | – | – | 26.58 |

**Per-round breakdown (mean ms, pooled over all ratios), and p95 of total_round_ms**

| system | outcome | n | session start | 16 commands | finalize | total | total p95 |
|---|---|---:|---:|---:|---:|---:|---:|
| Git + bwrap | commit | 250 | 109.5 | 92.0 | 102.6 | 304.1 | 321.4 |
| Git + bwrap | abort | 250 | 109.6 | 92.2 | 108.1 | 309.9 | 331.3 |
| raw Sandlock (1 run, unified) | commit | 250 | 0.0 | 58.5 | 27.4 | 85.9 | 93.2 |
| raw Sandlock (1 run, unified) | abort | 250 | 0.0 | 57.6 | 1.3 | 58.9 | 64.8 |
| cbcow-through | commit | 250 | 34.1 | 130.0 | 1.4 | 165.4 | 177.6 |
| cbcow-through | abort | 250 | 32.9 | 130.2 | 4.8 | 167.9 | 180.1 |
| cbcow-through durable | commit | 250 | 167.7 | 351.6 | 133.5 | 652.8 | 773.3 |
| cbcow-through durable | abort | 250 | 166.8 | 349.6 | 351.1 | 867.5 | 988.4 |
| cbcow-private | commit | 250 | 58.5 | 128.8 | 78.2 | 265.5 | 277.3 |
| cbcow-private | abort | 250 | 57.2 | 128.6 | 5.1 | 190.9 | 201.8 |
| cbcow-private durable | commit | 250 | 2732.8 | 361.0 | 791.2 | 3885.0 | 4774.0 |
| cbcow-private durable | abort | 250 | 2708.9 | 359.3 | 360.4 | 3428.6 | 4289.7 |
| raw Sandlock (16 runs) [session-reuse ref] | commit | 100 | 0.0 | 124.5 | 141.3 | 265.8 | 414.4 |

**Break-even commit ratio** (Lite faster than the baseline at commit ratios on the side shown)

| DeltaBox Lite | vs | measured crossover | model crossover | Lite − baseline at 0% / 100% (s) |
|---|---|---|---|---|
| cbcow-through | Git + bwrap | Lite faster at every ratio | none in 0–100% | -14.17 / -13.60 |
| cbcow-through | raw Sandlock (1 run, unified) | baseline faster at every ratio | none in 0–100% | +10.93 / +8.08 |
| cbcow-through durable | Git + bwrap | baseline faster at every ratio | none in 0–100% | +55.84 / +34.93 |
| cbcow-through durable | raw Sandlock (1 run, unified) | baseline faster at every ratio | none in 0–100% | +80.94 / +56.61 |
| cbcow-private | Git + bwrap | Lite faster at every ratio | none in 0–100% | -12.11 / -3.77 |
| cbcow-private | raw Sandlock (1 run, unified) | baseline faster at every ratio | none in 0–100% | +12.99 / +17.91 |
| cbcow-private durable | Git + bwrap | baseline faster at every ratio | none in 0–100% | +308.50 / +359.01 |
| cbcow-private durable | raw Sandlock (1 run, unified) | baseline faster at every ratio | none in 0–100% | +333.60 / +380.69 |
