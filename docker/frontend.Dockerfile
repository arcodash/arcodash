# syntax=docker/dockerfile:1
# Build from the repo root: docker build -f docker/frontend.Dockerfile .

FROM node:24-alpine AS build
WORKDIR /app
RUN corepack enable
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml ./
COPY apps/frontend/package.json apps/frontend/package.json
RUN pnpm install --frozen-lockfile
COPY apps/frontend apps/frontend
ARG VITE_API_BASE_URL=http://localhost:8080
ENV VITE_API_BASE_URL=${VITE_API_BASE_URL}
# Optional UI extension module, relative to apps/frontend.
ARG ARCODASH_EXT=""
ENV ARCODASH_EXT=${ARCODASH_EXT}
RUN pnpm --filter @arcodash/frontend build

FROM nginx:alpine AS runtime
COPY docker/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /app/apps/frontend/dist /usr/share/nginx/html
EXPOSE 80
