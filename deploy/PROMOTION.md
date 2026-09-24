# Release promotion

Fab builds each `main` commit once as a digest-pinned OCI image. The release
controller promotes that same artifact through its configured stages.

Marbles uses Shroud's compatibility-attested stateful rollout API. A rollout
retains the existing `marbles-company-store` volume, waits for `/healthz`, and
restores the previous image against the same bytes if the candidate is not
ready. Do not replace this with a retire-then-create deployment or a mutable
image tag.
