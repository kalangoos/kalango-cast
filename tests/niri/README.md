# niri para os testes do Kalango Cast

O niri aninhado (backend winit, numa janela) não fazia screencast: o PipeWire precisa de um dispositivo GBM para alocar
os quadros, e só o backend tty tinha um ("no GBM device available"). O patch
`0012-kalangoos-screencast-na-janela.patch` abre um GBM no nó de renderização da GPU em que a janela desenha (o mesmo
do EGL), e assim `RecordMonitor`/`RecordWindow` funcionam numa sessão aninhada, com DMA-BUF e modificadores como numa
sessão de verdade. Ele também aceita `KALANGO_NIRI_WINIT_SIZE=LxA` para o tamanho inicial da janela (testes de
desempenho em 1080p ou no tamanho de um monitor).

O patch vale sobre o niri do KalangoOS (a versão-base de `sessao/niri/preparar.sh` com os patches 0001–0011 do
repositório do KalangoOS). Para compilar:

```sh
# a árvore do niri com os patches do KalangoOS (sessao/niri/preparar.sh, no repositório do KalangoOS), depois:
git apply 0012-kalangoos-screencast-na-janela.patch
cargo build --release
export KALANGO_CAST_NIRI=$PWD/target/release/niri
```

Com `KALANGO_CAST_NIRI` definido, `cargo test -p cast-testkit --test niri_session -- --nocapture` abre uma sessão
niri numa janela do display atual (`DISPLAY`, ou `KALANGO_CAST_DISPLAY`), grava e confere os arquivos.
