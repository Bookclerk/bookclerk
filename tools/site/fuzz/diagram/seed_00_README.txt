flowchart LR
  subgraph Storefronts
    Audible
    Libro["Libro.fm"]
    Chirp
    GA["GraphicAudio"]
    ExtSource["external source plugins*"]
  end
  Audible & Libro & Chirp & GA & ExtSource --> Scan["scan"]
  Scan --> DB[("library.db")]
  DB --> Acquire["acquire (+ pack)"]
  subgraph Destinations
    Local["local filesystem"]
    S3["S3 / MinIO"]
    ExtOut["output plugins*"]
  end
  Acquire --> Local & S3 & ExtOut
  Services["naming / enrich / search"] --> DB
  Services --> Int["integrations<br/>(Audiobookshelf, SPA Accounts, …)"]
  Services --> Jobs["bookclerkd jobs +<br/>HTTP control plane"]
