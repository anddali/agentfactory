# AWS adapter setup

The same two binaries use ECS standalone Fargate tasks, RDS PostgreSQL and S3. These examples are deployment inputs, not provisioned resources. No AWS resources are created by local setup.

1. Build an approved worker image containing the required repository toolchain and your agent executable. Push it to ECR and record the image digest. Register a task definition based on `worker-task.example.json`. Its container must be named `worker`; configure a task definition revision, not a mutable family name.
2. Supply a production configuration based on `config/production.example.yaml`. Copy the three production workflows into `/app/production-workflows`, excluding the fixture demo. Build the server image with those files and `--build-arg FACTORY_BUILD_REVISION=<git-commit>`.
3. Create RDS PostgreSQL accessible only to the control plane, an S3 artifact bucket with encryption, versioning, public access blocked, and retention appropriate to your audit requirements. Keep RDS backups and retain artifact versions for at least as long as job records.
4. Use `server-task.example.json` for the ECS service. `DATABASE_URL` should require TLS, e.g. `postgres://.../factory?sslmode=require`. Keep `FACTORY_WORKER_SECRET` stable across server deployments; rotating it immediately revokes active worker callbacks.
5. Put the service behind a TLS listener. Configure the internal HTTPS URL in `FACTORY_WORKER_SERVER_URL`; allow workers to reach it and approved repository/agent endpoints. Use private subnets with the required NAT or VPC endpoints. Expose connector callback routes only where needed.

The server task role needs `ecs:RunTask` on approved worker task definitions, `ecs:DescribeTaskDefinition`, `ecs:ListTasks`, `ecs:StopTask` on its worker tasks, `iam:PassRole` on the exact worker task and execution roles, and `s3:GetObject`/`s3:PutObject` on `BUCKET/sha256/*`. The execution roles need ECR pull, log delivery, and only the Secrets Manager ARNs declared on their task definitions. The worker task role needs no database, S3, or control-plane IAM permissions: it receives a fenced callback token and uploads/downloads artifacts through authenticated endpoints.

Repository and agent secrets are supplied to the server through managed secrets, then delivered only in the claiming worker's HTTPS manifest according to its phase permissions. They are not stored in job snapshots, receipts, dispatch records, or ECS environment overrides. The agent subprocess gets its declared agent credentials; Git credentials are supplied only to Git/provider tasks. Use separate read and write credentials scoped to each registered repository. The platform cannot turn a broad provider token into a narrowly scoped token, so provider-side scopes must match the policy.

Fargate does not support privileged containers. Repositories that need Docker-in-Docker or privileged integration tests require another approved execution profile and executor implementation. Do not mount the Docker socket into production Fargate tasks. The Docker socket mount in Compose is exclusively for the local control plane.

References: [ECS standalone tasks](https://docs.aws.amazon.com/AmazonECS/latest/developerguide/standalone-tasks.html), [RunTask](https://docs.aws.amazon.com/AmazonECS/latest/APIReference/API_RunTask.html), [Fargate security considerations](https://docs.aws.amazon.com/AmazonECS/latest/developerguide/fargate-security-considerations.html).

