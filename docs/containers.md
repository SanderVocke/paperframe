# Containers and Kubernetes

The repository includes a production multi-stage [`Dockerfile`](../Dockerfile), safe container defaults, and an example Kubernetes deployment.

## Container contract

The image:

- listens on port `8080`;
- runs as the non-root user and group `10001:10001`;
- stores writable settings, queue state, and managed JPEG copies under `/data`;
- reads bootstrap configuration from `/config/config.toml`;
- expects a manual photo folder at `/photos` with the default settings;
- handles `SIGTERM` for graceful shutdown; and
- reports process health at `/health/live` and frame readiness at `/health/ready`.

On the first start of a new `/data` volume, the entrypoint creates `/data/settings.toml` from the bundled manual-folder defaults. It never overwrites an existing settings file. Changes made in the web app are therefore retained when `/data` is persistent.

Source photos and DigiKam files should always be mounted read-only. Only `/data` needs write access.

## Build and run with Docker

Build the image from the repository root:

```sh
docker build --pull -t paperframe-server:local .
```

Run it with a normal photo folder:

```sh
docker volume create paperframe-data

docker run -d \
  --name paperframe \
  --restart unless-stopped \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=268435456 \
  -p 8080:8080 \
  -v paperframe-data:/data \
  -v /absolute/path/to/photos:/photos:ro \
  paperframe-server:local
```

Open `http://DOCKER_HOST:8080/`. The default policy is manual, so browser testing does not unexpectedly advance the frame.

The named `paperframe-data` volume contains both the queue and the settings changed through the web app. Back up the entire volume together and do not mount it into more than one running container.

### Run with DigiKam

Mount the directory containing both the DigiKam database and collection. For the archive layout used in the example Kubernetes manifest:

```sh
docker run -d \
  --name paperframe \
  --restart unless-stopped \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=268435456 \
  -p 8080:8080 \
  -v paperframe-data:/data \
  -v /mnt/data/digikam:/library:ro \
  paperframe-server:local
```

Then open the web app, select **DigiKam library**, and configure:

- database: `/library/digikam_db/digikam4.db`;
- the database root exactly as stored in DigiKam's `AlbumRoots.specificPath`;
- mounted root: `/library/collection`; and
- the desired read-only selection query.

Alternatively, prepare a writable host directory containing `settings.toml` and mount that directory at `/data`. Ensure UID/GID `10001:10001` can write it:

```sh
mkdir -p runtime/queue
cp settings.example.toml runtime/settings.toml
# Edit paths so they refer to container paths such as /library/...
sudo chown -R 10001:10001 runtime

docker run --rm -p 8080:8080 \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=268435456 \
  -v "$PWD/runtime:/data" \
  -v /mnt/data/digikam:/library:ro \
  paperframe-server:local
```

For a live SQLite catalog using WAL mode, mount a consistent database/WAL/SHM set or use a safe catalog snapshot.

## Publish to an online registry

For a single architecture:

```sh
docker login ghcr.io
docker build -t ghcr.io/YOUR_USER/paperframe-server:0.1.0 .
docker push ghcr.io/YOUR_USER/paperframe-server:0.1.0
```

To publish both common home-server architectures with Buildx:

```sh
docker buildx create --name paperframe-builder --use 2>/dev/null || \
  docker buildx use paperframe-builder

docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --tag ghcr.io/YOUR_USER/paperframe-server:0.1.0 \
  --tag ghcr.io/YOUR_USER/paperframe-server:latest \
  --push .
```

Use immutable version tags or image digests in Kubernetes for repeatable deployments. If the registry is private, configure an `imagePullSecret` in the usual way for your cluster.

## Kubernetes

[`deploy/kubernetes/paperframe.yaml`](../deploy/kubernetes/paperframe.yaml) provides:

- a ConfigMap with bootstrap settings and a DigiKam settings seed;
- a 5 GiB persistent claim for `/data`;
- a single-replica Deployment with `Recreate` strategy;
- an init container that seeds settings only when the data claim is new;
- non-root, read-only-root-filesystem security settings;
- startup, liveness, and readiness probes; and
- a ClusterIP Service on port `8080`.

### 1. Prepare the image and library storage

Replace both `ghcr.io/YOUR_USER/paperframe-server:latest` values in the manifest with your published image tag.

The example expects a PVC named `digikam-library` whose root has this shape:

```text
digikam-library PVC
├── digikam_db/
│   └── digikam4.db
└── collection/
    └── ... photos ...
```

How that PVC is provided depends on the cluster. NFS, a CSI driver, and a pre-provisioned local volume are all valid choices. It is mounted read-only by the pod.

For a single-node home cluster, you can instead replace the `library` volume in the Deployment with a host path:

```yaml
- name: library
  hostPath:
    path: /mnt/data/digikam
    type: Directory
```

A `hostPath` ties the pod to a node. Use node affinity if the workload could otherwise be scheduled elsewhere.

### 2. Adjust the DigiKam seed

In the ConfigMap:

- change `database_path` if the database is elsewhere in the mounted library;
- set `database_root` to the exact path returned by DigiKam's `AlbumRoots.specificPath`;
- set `mounted_root` to the corresponding path inside the container; and
- customize the query if desired.

The supplied query chooses active JPEG files in stable image-ID order and limits the catalog scan to 100,000 candidates.

The ConfigMap settings are copied only when `/data/settings.toml` does not exist. This is intentional: subsequent web-app changes live on the PVC and survive pod replacements. Editing `settings.toml` in the ConfigMap does not replace settings on an existing data claim. Use the web app, or deliberately update the file on the PVC while the Deployment is stopped.

### 3. Deploy and inspect

```sh
kubectl apply -f deploy/kubernetes/paperframe.yaml
kubectl rollout status deployment/paperframe
kubectl get pods,service,pvc -l app.kubernetes.io/name=paperframe
kubectl logs deployment/paperframe
```

For an initial local check:

```sh
kubectl port-forward service/paperframe 8080:8080
```

Then open `http://127.0.0.1:8080/`.

`/health/ready` returns `503` while no image is available, so the Service may temporarily have no ready endpoints during initial import or a source error. If needed, port-forward directly to the pod to inspect the control page:

```sh
kubectl port-forward pod/$(kubectl get pod \
  -l app.kubernetes.io/name=paperframe \
  -o jsonpath='{.items[0].metadata.name}') 8080:8080
```

### 4. Expose it on the home network

Keep the supplied ClusterIP Service when using an Ingress or Gateway. Point the frame at:

```text
https://YOUR_PRIVATE_HOST/frame/current.jpg
```

The application itself has no authentication or TLS. Do not publish it directly to the internet. Use a private LAN/VPN and, for browser access, an authenticating HTTPS ingress if possible. If you change the Service to `LoadBalancer` or `NodePort`, restrict it to trusted networks with firewall rules or network policy.

## Operations

- Keep `replicas: 1`; queue storage is not designed for concurrent writers.
- Preserve the `Recreate` deployment strategy to avoid overlapping old and new pods.
- Back up all of `/data`, not just individual JPEGs or `settings.toml`.
- Keep the source library read-only.
- Give termination at least 30 seconds so in-flight responses and state changes can finish.
- When upgrading, publish a new immutable image tag, change the Deployment image, and use `kubectl rollout status deployment/paperframe`.
- If a bind mount is not writable, fix ownership/permissions for UID/GID `10001`, or run the container with a deliberate alternate UID/GID that can write `/data` and read the source mounts.
