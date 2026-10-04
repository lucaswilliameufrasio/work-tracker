# Puxiri

Gestor de trabalho self hosted em desenvolvimento. Uma instalação, uma organização, SQLite local e frontend embutido no executável Rust.

**Esta versão é um spike da Fase 0, não um produto pronto.** Apenas servidor, migrations, lock do diretório, saúde mínima e SPA funcionam. Login, setup protegido, Kanban, integrações e backup ainda não estão implementados. Não exponha na internet nem use dados reais.

## Desenvolvimento local

Requisitos: Rust 1.94 ou superior, Node 24 e pnpm 11.21.0. Node é necessário para construir/testar a SPA, não para executar o binário pronto.

```sh
make install
make serve
```

Abra `http://127.0.0.1:3000`. Dados de desenvolvimento ficam em `./data`, ignorado pelo Git. Uma segunda instância no mesmo diretório é rejeitada.

Para reconstruir e gerar o executável:

```sh
make build
./target/release/puxiri serve --bind 127.0.0.1:3000 --data-dir ./data
```

O binário contém HTML, JS, CSS, fontes e migrations. Pode ser executado fora do repositório, sem Node nem `web/dist`. SQLite/WAL/SHM devem permanecer em disco local, nunca NFS/S3. O binário deste spike ainda depende de bibliotecas do sistema; distribuição portátil será validada posteriormente.

Opções de bootstrap implementadas: `--bind`/`PUXIRI_BIND` e `--data-dir`/`PUXIRI_DATA_DIR`. `RUST_LOG` controla os logs estruturados. Não passar segredos em URLs/argumentos. Encerre com Ctrl+C ou SIGTERM para fechar conexões graciosamente.

## Validação

```sh
(cd web && pnpm exec playwright install chromium)
make check
```

Integrações usam SQLite em arquivos temporários reais. E2E inicia o executável em um diretório isolado e verifica a SPA embutida no desktop e mobile escuro, sem servidor Node/Vite de produção. O gate também exige Docker para o spike S3: inicia MinIO descartável com dados em tmpfs e porta aleatória só no loopback, e remove apenas seu próprio container ao terminar. Nenhum endpoint, bucket ou credencial de outro ambiente é utilizado. O registry pode ser acessado para obter a imagem fixada.

`make test-s3` roda apenas os contratos de armazenamento/URLs/CORS em MinIO real e Chromium, com SDK S3 oficial usado só no harness Node de desenvolvimento. Não existe adaptador S3 do produto ainda. A imagem `cgr.dev/chainguard/minio` está fixada por digest em `web/scripts/s3-spike.test.mjs`; disponibilidade para pull precisa ser verificada no ambiente de CI. O teste falha, não pula a integração, se Docker/imagem/Chromium não estiverem disponíveis.

O mesmo spike usa SQLite em arquivo temporário via `node:sqlite` somente no harness para exercitar claims, fencing, rollback e pins remotos. Um processo filho exclusivo é interrompido por SIGKILL após PUT e antes do commit, verificando recuperação por readback sem staging. Isso não muda SQLx/Rust do servidor nem implementa o fluxo autenticado de uploads. Estado temporário fica em `.test-data` e é removido pelo teste.

O harness também valida bundle TAR/AES-GCM experimental (limite de fixture: 8 MiB), snapshot com pins, inventário/checksums e restore local isolado após apagar o bucket de origem. Usa chave efêmera de teste, sem gestão de chaves, retenção ou ativação de instalação restaurada. Spools plaintext ficam em diretórios privados de teste; não é formato público nem backup/restore de produção.

Jobs experimentais de bundle persistem estado/leases e pins. Testes interrompem um filho por SIGKILL em quatro fronteiras; recuperação só libera pins depois de o supervisor confirmar que o worker parou, nunca apenas por expiração de lease. Limpeza de spool/partial é recuperável e preserva bundles completos e diagnósticos incompletos. Ainda não há supervisor Rust, retomada automática nem política de retenção de diagnósticos.

`make measure-snapshot` mede localmente uma fixture Rust de 100 mil registros/10 mil objetos. `make measure-resources` repete em containers limitados a 1 CPU/1 GiB e 2 CPU/2 GiB, verifica cotas efetivas e guarda JSON em `.test-data/reports`. Mede freeze, Online Backup com espera de escritas, WAL/checkpoint, transações sintéticas, RSS e pico do cgroup; não mede HTTP/Argon2/capacidade real. Usa runtime Rust fixado por digest em `web/scripts/measure-resources.mjs` e bind volume privado (não tmpfs). Docker rootless mantém volume 0700 com UID mapeado do host.

`make check` também exige a imagem Node fixada em `web/scripts/s3-spool-budget-contracts.mjs` para testar preflight de disco em tmpfs real de 4 MiB. Bundle experimental recusa excesso de inventário/bytes ou espaço insuficiente antes de copiar banco/inserir pins. Preflight não é reserva global de espaço e não elimina ENOSPC durante execução. Pull/arquiteturas das imagens fixadas ainda precisam de validação em CI.

Endpoints públicos deste spike: `/health/live` e `/health/ready`. Readiness retorna apenas `ready`/`not_ready`; não expõe caminhos, versão, usuários ou conteúdo.

## Plano e distribuição

[Plano de desenvolvimento](docs/development-plan.md). O restante da Fase 0 precisa validar backup online, OAuth/MCP nos quatro clientes, providers de email e S3 privado. Nada disso é considerado pronto pelo esqueleto.

Direitos reservados. Repositório privado, sem licença pública; não anunciado como open source. Licença e condições serão decididas antes da distribuição pública.
