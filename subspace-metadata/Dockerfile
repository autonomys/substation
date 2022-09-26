# Install dependencies
FROM node:16 as dependencies

RUN mkdir -p /metadata
WORKDIR /metadata

COPY package.json /metadata
COPY yarn.lock /metadata
RUN yarn

COPY . /metadata
RUN yarn build

RUN yarn cache clean

EXPOSE 8080
CMD ["node", "dist/index.js"]
