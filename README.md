# Puxiri

Gestor de trabalho self-hosted: uma instalação, uma organização, SQLite local e a interface embutida num único executável Rust.

> **Estado:** primeira versão utilizável, ainda em desenvolvimento. Mantenha a replicação ligada. O que falta está no [plano de desenvolvimento](docs/development-plan.md).

## O que já funciona

- **Projetos e issues:** quadro Kanban, título e descrição, prioridade, prazo, responsável, busca com filtros, comentários e a vista "meus itens".
- **Entrada:** setup com token de uso único, login por um provedor OIDC (Muira) ou pela autenticação própria (incompleta), convites por email (Sabiá, MillionSend ou Resend) e sessões com MFA.
- **MCP** em `/mcp`, autorizado pelo Puxiri.
- **Administração** em `/gestao`: pessoas, banimento, projetos, remetente de email, auditoria e replicação.
- **Recuperação de desastres** com [`sqlite-replication`](https://github.com/lucaswilliameufrasio/sqlite-replication): captura automática, envio opcional para S3 e restauração em arquivo novo.

Ainda não tem labels, campos customizados, atualização ao vivo, anexos nem automações.

## Rodar com Docker Compose

`compose.yaml` sobe só o Puxiri. Não há TLS: coloque um HTTPS seu na frente da porta publicada.

```sh
cp .env.example .env                              # preencha PUXIRI_BROWSER_ORIGIN
docker compose build                              # precisa de ../sqlite-replication
docker compose run --rm -it puxiri setup-token    # primeira instalação, com o serviço parado
docker compose up -d
```

Imagem publicada, para não compilar: `ghcr.io/lucaswilliameufrasio/puxiri:<versão>` (só `linux/amd64`). Para deploy pelo Uirapuru, use `deploy/uirapuru/compose.yaml` com estratégia `recreate` e uma réplica: o SQLite aceita uma instância só. Detalhes no [ADR 0009](docs/adr/0009-secrets-from-the-environment-and-uirapuru-deploy.md).

## Configuração

Tudo vem do ambiente, segredos inclusive. Ciphera, Infisical e Vault entram pelo `run` ou pelo agente de cada um.

| Variável | Padrão | Para quê |
| --- | --- | --- |
| `PUXIRI_BROWSER_ORIGIN` | — | Origem HTTPS pública. Sem ela, a interface autenticada fica desligada. |
| `PUXIRI_BIND` | `127.0.0.1:3000` | Endereço de escuta (`0.0.0.0:3000` na imagem). |
| `PUXIRI_DATA_DIR` | `data` | Banco, réplica local e restaurações. |
| `PUXIRI_MASTER_KEY_BASE64` | — | Master key (32 bytes em base64). Sem ela, é criada em `master.key` no diretório de dados. |
| `PUXIRI_REPLICATION_INTERVAL_SECONDS` | `300` | Intervalo da captura automática; `0` desliga. |
| `PUXIRI_REPLICATION_GENERATION_HOURS` | `24` | Idade em que começa uma geração nova, a partir de um snapshot. |
| `PUXIRI_REPLICATION_RETAIN_GENERATIONS` | `7` | Gerações mantidas no S3. |
| `PUXIRI_REPLICATION_S3_BUCKET` | — | Liga o envio para S3. Também: `_PREFIX`, `_REGION`, `_ENDPOINT`, `_PATH_STYLE`. |
| `PUXIRI_REPLICATION_S3_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY` | — | Credenciais do S3, só por ambiente. |
| `RUST_LOG` | `info` | Nível dos logs (JSON). |

**Guarde a master key fora do servidor.** O S3 recupera o banco, mas os segredos dentro dele (TOTP, chaves de email) só abrem com a mesma chave. Para exportar a de uma instalação existente: `docker compose cp puxiri:/data/master.key - | tar -xO | base64 -w0`.

Saúde: `GET /health/live` e `GET /health/ready`.

## Desenvolvimento

Versões fixadas em `.mise.toml`: Rust 1.99.1, Node 24.21.0, pnpm 11.21.0. Clone o `sqlite-replication` ao lado deste repositório.

```sh
mise install
mise exec -- make install    # dependências da interface
mise exec -- make serve      # http://127.0.0.1:3000, dados em ./data
mise exec -- make check      # formatação, lint, testes Rust e web, E2E e contratos S3 (precisa de Docker)
make test-replication-s3     # replicação contra um RustFS descartável
```

## Documentação

- [Plano de desenvolvimento](docs/development-plan.md): fases, decisões e o que falta.
- [Decisões (ADRs)](docs/adr/): v1 utilizável com Muira (0006), recuperação e administração (0007), S3 e gerações (0008), segredos e Uirapuru (0009).
- [Spikes e validações da Fase 0](docs/validation-spikes.md): o que cada harness de teste cobre e o que deixa de fora.
- Desenho de cada área em [`docs/`](docs/).

## Licença

Sem licença por enquanto: o código está público para leitura, mas todos os direitos são reservados.
