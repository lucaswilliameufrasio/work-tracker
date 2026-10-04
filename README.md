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

Endpoints públicos deste spike: `/health/live` e `/health/ready`. Readiness retorna apenas `ready`/`not_ready`; não expõe caminhos, versão, usuários ou conteúdo.

## Plano e distribuição

[Plano de desenvolvimento](docs/development-plan.md). O restante da Fase 0 precisa validar backup online, OAuth/MCP nos quatro clientes, providers de email e S3 privado. Nada disso é considerado pronto pelo esqueleto.

Direitos reservados. Repositório privado, sem licença pública; não anunciado como open source. Licença e condições serão decididas antes da distribuição pública.
